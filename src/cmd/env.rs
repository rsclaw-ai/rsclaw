//! `rsclaw env …` command handlers.
//!
//! Operates on the auto-managed `$RSCLAW_BASE_DIR/.env` file driven by
//! `config::env_resolution`. The runtime auto-syncs on every gateway
//! load; these commands let operators inspect / force-sync without
//! restarting.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::cli::{EnvCommand, EnvSyncArgs};
use crate::config::{env_file, env_resolution, loader};

pub async fn cmd_env(sub: EnvCommand) -> Result<()> {
    match sub {
        EnvCommand::Sync(args) => env_sync(args).await,
        EnvCommand::List => env_list().await,
    }
}

async fn env_sync(args: EnvSyncArgs) -> Result<()> {
    let base_dir = loader::base_dir();
    let env_path = base_dir.join(".env");

    let config_path = loader::detect_config_path()
        .ok_or_else(|| anyhow::anyhow!("no rsclaw config file found"))?;
    let raw = std::fs::read_to_string(&config_path)?;
    let needed = env_resolution::scan_var_refs(&raw);

    let shell = env_resolution::shell_snapshot();
    let mut file = env_file::read(&env_path)?;

    let mut added: Vec<String> = Vec::new();
    let mut updated: Vec<(String, String, String)> = Vec::new(); // (name, old, new)
    let mut still_missing: Vec<String> = Vec::new();
    let mut overwritten_blanks: Vec<String> = Vec::new();

    for var in &needed {
        match (shell.get(var), file.get(var)) {
            (Some(s), Some(f)) if s != f => {
                if args.force || !s.is_empty() {
                    updated.push((var.clone(), redact(f), redact(s)));
                    if !args.dry_run {
                        file.insert(var.clone(), s.clone());
                    }
                }
            }
            (Some(s), None) => {
                added.push(var.clone());
                if !args.dry_run {
                    file.insert(var.clone(), s.clone());
                }
            }
            (None, Some(_)) if args.force => {
                // --force clears entries the shell no longer has.
                overwritten_blanks.push(var.clone());
                if !args.dry_run {
                    file.insert(var.clone(), String::new());
                }
            }
            (None, None) => still_missing.push(var.clone()),
            _ => {}
        }
    }

    let file_changed = !added.is_empty() || !updated.is_empty() || !overwritten_blanks.is_empty();
    if file_changed && !args.dry_run {
        env_file::write(&env_path, &file)?;
    }

    // Report.
    println!("rsclaw env sync — {}", env_path.display());
    println!("  config:    {}", config_path.display());
    println!("  vars used: {}", needed.len());
    if !added.is_empty() {
        println!("\n  added ({}):", added.len());
        for v in &added {
            println!("    + {v}");
        }
    }
    if !updated.is_empty() {
        println!("\n  updated ({}, shell wins):", updated.len());
        for (n, _old, _new) in &updated {
            println!("    ~ {n}");
        }
    }
    if !overwritten_blanks.is_empty() {
        println!("\n  blanked ({}, --force):", overwritten_blanks.len());
        for v in &overwritten_blanks {
            println!("    - {v}");
        }
    }
    if !still_missing.is_empty() {
        println!("\n  still missing ({}):", still_missing.len());
        for v in &still_missing {
            println!("    ? {v}");
        }
        println!(
            "\n    Set these in your shell (e.g. ~/.zshrc) and re-run, or edit"
        );
        println!("    {} directly.", env_path.display());
    }
    if !file_changed && still_missing.is_empty() {
        println!("\n  nothing to do — .env is in sync with shell + config.");
    } else if args.dry_run {
        println!("\n  (dry-run — no changes written)");
    }
    Ok(())
}

async fn env_list() -> Result<()> {
    let base_dir = loader::base_dir();
    let env_path = base_dir.join(".env");

    let config_path = loader::detect_config_path()
        .ok_or_else(|| anyhow::anyhow!("no rsclaw config file found"))?;
    let raw = std::fs::read_to_string(&config_path)?;
    let needed = env_resolution::scan_var_refs(&raw);

    let shell = env_resolution::shell_snapshot();
    let file = env_file::read(&env_path)?;

    // Header.
    println!("rsclaw env list — {}", config_path.display());
    println!("  .env: {}", env_path.display());
    println!("  vars referenced: {}", needed.len());
    if needed.is_empty() {
        return Ok(());
    }
    println!();

    let name_w = needed.iter().map(String::len).max().unwrap_or(20).max(20);
    println!(
        "  {:<width$}  shell      .env       status",
        "VAR",
        width = name_w
    );
    println!(
        "  {:<width$}  ---------  ---------  ------",
        "---",
        width = name_w
    );
    for var in &needed {
        let in_shell = shell.contains_key(var);
        let in_file = file.contains_key(var);
        let status = match (in_shell, in_file) {
            (true, true) if shell.get(var) == file.get(var) => "ok",
            (true, true) => "drift",
            (true, false) => "shell-only",
            (false, true) => "file-only",
            (false, false) => "MISSING",
        };
        let shell_mark = if in_shell { "set      " } else { "         " };
        let file_mark = if in_file { "set      " } else { "         " };
        println!(
            "  {:<width$}  {}  {}  {}",
            var,
            shell_mark,
            file_mark,
            status,
            width = name_w
        );
    }

    // Drop unused binding so clippy doesn't complain about
    // `let _ = file;` style.
    let _ = BTreeMap::<String, String>::new();
    Ok(())
}

fn redact(s: &str) -> String {
    if s.len() <= 8 {
        "***".to_owned()
    } else {
        format!("{}...{}", &s[..4], &s[s.len() - 4..])
    }
}
