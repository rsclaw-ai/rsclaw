use anyhow::Result;

use super::style::*;
use crate::{cli::SecurityCommand, config};

pub async fn cmd_security(sub: SecurityCommand) -> Result<()> {
    match sub {
        #[cfg(unix)]
        SecurityCommand::Audit(args) => cmd_security_audit(args).await,
        #[cfg(not(unix))]
        SecurityCommand::Audit(_) => {
            anyhow::bail!("security audit is only supported on Unix-like systems");
        }
    }
}

// ---------------------------------------------------------------------------
// security audit (AGENTS.md S24)
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn cmd_security_audit(args: crate::cli::SecurityAuditArgs) -> Result<()> {
    banner(&format!("rsclaw security audit v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));

    let base = crate::config::loader::base_dir();
    let mut issues: Vec<String> = Vec::new();
    let mut fixed = 0usize;

    // 1. Config file permissions -- must not be world-readable.
    for cfg_path in [
        base.join("rsclaw.json5"),
    ] {
        if !cfg_path.exists() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let meta = std::fs::metadata(&cfg_path)?;
            let mode = meta.permissions().mode();
            if mode & 0o044 != 0 {
                let issue = format!(
                    "config {} is world/group-readable (mode {:04o})",
                    cfg_path.display(),
                    mode & 0o777
                );
                if args.fix {
                    std::fs::set_permissions(
                        &cfg_path,
                        std::fs::Permissions::from_mode(mode & !0o077),
                    )?;
                    ok(&format!("fixed: {issue}"));
                    fixed += 1;
                } else {
                    issues.push(issue);
                }
            }
        }
        #[cfg(windows)]
        {
            // Windows ACL: check config is not in a shared/public directory.
            let cfg_dir = cfg_path.parent().unwrap_or(std::path::Path::new("."));
            if let Some(public) = std::env::var_os("PUBLIC") {
                if cfg_dir.starts_with(std::path::Path::new(&public)) {
                    issues.push(format!(
                        "config {} is under the Public folder -- move to %LOCALAPPDATA%",
                        cfg_path.display()
                    ));
                }
            }
        }
    }

    // 2. Scan config for plaintext API keys (not ${VAR} references).
    for cfg_path in [base.join("rsclaw.json5")] {
        if !cfg_path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&cfg_path)?;
        for (lineno, line) in raw.lines().enumerate() {
            let lower = line.to_lowercase();
            // Heuristic: value contains "sk-" or "key-" and is not a ${VAR}.
            if (lower.contains("api_key") || lower.contains("apikey") || lower.contains("token"))
                && line.contains('=')
                && !line.contains("${")
                && !line.trim_start().starts_with('#')
            {
                let rhs = line.split_once('=').map_or("", |(_, v)| v).trim();
                // Ignore empty values, placeholder strings, and quoted empty.
                if rhs.is_empty() || rhs == "\"\"" || rhs == "''" {
                    continue;
                }
                issues.push(format!(
                    "{}:{} may contain a plaintext secret: {}",
                    cfg_path.display(),
                    lineno + 1,
                    &line[..line.len().min(60)]
                ));
            }
        }
    }

    // 3. Report results.
    if issues.is_empty() && fixed == 0 {
        ok("no issues found");
    } else {
        if fixed > 0 {
            ok(&format!("fixed {} issue(s)", bold(&fixed.to_string())));
        }
        if !issues.is_empty() {
            err_msg(&format!("{} issue(s) found:", issues.len()));
            for issue in &issues {
                println!("    - {}", red(issue));
            }
            if !args.fix {
                println!();
                println!("  {}", dim("Run with --fix to auto-correct where possible."));
            }
        }
    }

    if args.deep {
        // Deep scan: walk state dir for files with overly broad permissions.
        #[cfg(unix)]
        {
            let state = config::loader::base_dir();
            if state.is_dir() {
                scan_dir_permissions(&state, 0, 4, args.fix, &mut issues, &mut fixed)?;
            }
        }
        #[cfg(windows)]
        {
            println!("  {} deep permission scan skipped (Windows uses ACL, not mode bits)", dim("[--]"));
        }
    }

    Ok(())
}

#[cfg(unix)]
pub fn scan_dir_permissions(
    dir: &std::path::Path,
    depth: usize,
    max_depth: usize,
    fix: bool,
    issues: &mut Vec<String>,
    fixed: &mut usize,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    if depth > max_depth {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let path = entry.path();
        let mode = meta.permissions().mode();
        if meta.is_file() && mode & 0o044 != 0 {
            let issue = format!(
                "state file {} is group/world-readable (mode {:04o})",
                path.display(),
                mode & 0o777
            );
            if fix {
                if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & !0o077))
                    .is_ok()
                {
                    ok(&format!("fixed: {issue}"));
                    *fixed += 1;
                }
            } else {
                issues.push(issue);
            }
        } else if meta.is_dir() {
            scan_dir_permissions(&path, depth + 1, max_depth, fix, issues, fixed)?;
        }
    }
    Ok(())
}
