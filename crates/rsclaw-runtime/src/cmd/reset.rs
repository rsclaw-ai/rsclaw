use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

use anyhow::Result;
use rsclaw_cli::ResetArgs;
use rsclaw_config as config;

use super::style::*;

/// Canonicalize `p`, falling back to the path as given when it doesn't exist.
fn canonical_or_self(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Resolve the config file this instance would delete. Only a file that
/// lives under the resolved base dir qualifies, so `--profile x reset
/// --scope config` can never fall through to the default instance's config.
fn config_path_in_base(base_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(path) = config::loader::detect_config_path() else {
        return Ok(None);
    };
    let base = canonical_or_self(base_dir);
    let canon = canonical_or_self(&path);
    if !canon.starts_with(&base) {
        anyhow::bail!(
            "config file {} is outside this instance's state dir {}; refusing to delete it",
            path.display(),
            base_dir.display()
        );
    }
    Ok(Some(path))
}

/// Refuse to wipe the state dir if it resolves to something that is clearly
/// not an rsclaw state dir (filesystem root or the home directory itself).
fn ensure_safe_state_dir(base_dir: &Path) -> Result<()> {
    let canon = canonical_or_self(base_dir);
    if canon.parent().is_none() {
        anyhow::bail!("refusing to delete filesystem root {}", base_dir.display());
    }
    if let Some(home) = dirs_next::home_dir()
        && canonical_or_self(&home) == canon
    {
        anyhow::bail!("refusing to delete the home directory {}", base_dir.display());
    }
    Ok(())
}

/// Bail when this instance's gateway is running: deleting its state (redb,
/// config) under a live process corrupts it.
fn ensure_gateway_stopped() -> Result<()> {
    let pid = std::fs::read_to_string(crate::cmd::gateway::gateway_pid_file())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    if let Some(pid) = pid
        && rsclaw_platform::process_is_rsclaw(pid)
    {
        anyhow::bail!(
            "the gateway is running (pid {pid}); stop it first with `rsclaw gateway stop`"
        );
    }
    Ok(())
}

/// Ask the user to type `yes`. `--yes` skips the prompt; `--non-interactive`
/// without `--yes` refuses.
fn confirm(args: &ResetArgs, what: &str) -> Result<bool> {
    if args.yes {
        return Ok(true);
    }
    if args.non_interactive {
        anyhow::bail!("refusing to {what} without confirmation; pass --yes");
    }
    print!("  {} ", bold(&format!("{what}? Type 'yes' to continue:")));
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("yes"))
}

/// `rsclaw reset` — delete this instance's config (`--scope config`) or its
/// whole state dir (`--scope full`, default). Requires confirmation unless
/// `--yes`, and refuses while the gateway is running.
pub async fn cmd_reset(args: ResetArgs) -> Result<()> {
    let base_dir = config::loader::base_dir();

    let scope = args.scope.as_deref().unwrap_or("full");

    if args.dry_run {
        banner(&format!(
            "rsclaw reset (dry run) v{}",
            option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
        ));
        match scope {
            "config" => {
                if let Some(path) = config_path_in_base(&base_dir)? {
                    warn_msg(&format!(
                        "would remove config: {}",
                        bold(&path.display().to_string())
                    ));
                } else {
                    warn_msg("no config file found");
                }
            }
            "full" => {
                if base_dir.exists() {
                    ensure_safe_state_dir(&base_dir)?;
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

    banner(&format!(
        "rsclaw reset v{}",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));
    println!("  {}", red("WARNING: This is a destructive operation!"));
    println!();

    match scope {
        "config" => {
            if let Some(path) = config_path_in_base(&base_dir)? {
                ensure_gateway_stopped()?;
                println!(
                    "  {} {}",
                    red("will remove"),
                    bold(&path.display().to_string())
                );
                if !confirm(&args, "remove the config file")? {
                    warn_msg("aborted");
                    return Ok(());
                }
                std::fs::remove_file(&path)?;
                ok(&format!(
                    "removed config: {}",
                    dim(&path.display().to_string())
                ));
            } else {
                warn_msg("no config file found");
            }
        }
        "full" => {
            if base_dir.exists() {
                ensure_safe_state_dir(&base_dir)?;
                ensure_gateway_stopped()?;
                println!(
                    "  {} {}",
                    red("will remove"),
                    bold(&base_dir.display().to_string())
                );
                if !confirm(&args, "remove the entire state dir")? {
                    warn_msg("aborted");
                    return Ok(());
                }
                std::fs::remove_dir_all(&base_dir)?;
                ok(&format!(
                    "removed state dir: {}",
                    dim(&base_dir.display().to_string())
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
    Ok(())
}
