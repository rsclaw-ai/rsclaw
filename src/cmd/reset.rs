use anyhow::Result;

use super::style::*;
use crate::{
    cli::{ResetArgs, UpdateArgs, UpdateCommand},
    config,
};

pub async fn cmd_reset(args: ResetArgs) -> Result<()> {
    let base_dir = config::loader::base_dir();

    let scope = args.scope.as_deref().unwrap_or("full");

    if args.dry_run {
        banner(&format!(
            "rsclaw reset (dry run) v{}",
            env!("RSCLAW_BUILD_VERSION")
        ));
        match scope {
            "config" => {
                if let Some(path) = config::loader::detect_config_path() {
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

    banner(&format!("rsclaw reset v{}", env!("RSCLAW_BUILD_VERSION")));
    println!("  {}", red("WARNING: This is a destructive operation!"));
    println!();

    match scope {
        "config" => {
            if let Some(path) = config::loader::detect_config_path() {
                println!(
                    "  {} {}",
                    red("removing"),
                    bold(&path.display().to_string())
                );
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
                println!(
                    "  {} {}",
                    red("removing"),
                    bold(&base_dir.display().to_string())
                );
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

pub async fn cmd_update(sub: UpdateCommand) -> Result<()> {
    match sub {
        UpdateCommand::Run(args) => do_update(&args).await?,
        UpdateCommand::Status => update_status().await?,
        UpdateCommand::Wizard => {
            banner(&format!(
                "rsclaw update wizard v{}",
                env!("RSCLAW_BUILD_VERSION")
            ));
            warn_msg("update wizard: not yet implemented");
            println!("  {}", dim("use `rsclaw update run` for now"));
        }
    }
    Ok(())
}

const RSCLAW_VERSION_URL: &str = "https://app.rsclaw.ai/api/version";
const RELEASE_URL: &str = "https://api.github.com/repos/rsclaw-ai/rsclaw/releases/latest";

/// Apply GITHUB_PROXY env to a URL: proxy + "/" + url
fn proxy_url(url: &str) -> String {
    if let Ok(proxy) = std::env::var("GITHUB_PROXY") {
        let proxy = proxy.trim_end_matches('/');
        if !proxy.is_empty() {
            return format!("{}/{}", proxy, url);
        }
    }
    url.to_owned()
}

fn build_update_client(timeout_secs: u64) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("rsclaw/", env!("RSCLAW_BUILD_VERSION")))
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()?)
}

async fn do_update(args: &UpdateArgs) -> Result<()> {
    banner(&format!("rsclaw update v{}", env!("RSCLAW_BUILD_VERSION")));

    let timeout_secs = args.timeout.unwrap_or(30);
    let client = build_update_client(timeout_secs)?;

    // 1. Check latest release — try app.rsclaw.ai first, fallback GitHub
    println!("  {} checking for updates...", dim("[..]"));

    let release: serde_json::Value = {
        let mut data = None;
        // Primary: app.rsclaw.ai/api/version (same format as GitHub releases/latest)
        if let Ok(resp) = client.get(RSCLAW_VERSION_URL).send().await {
            if resp.status().is_success() {
                if let Ok(d) = resp.json::<serde_json::Value>().await {
                    if d.get("tag_name").is_some() {
                        data = Some(d);
                    }
                }
            }
        }
        // Fallback: GitHub releases API
        if data.is_none() {
            let resp = client.get(&proxy_url(RELEASE_URL)).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("failed to check for updates: {}", resp.status());
            }
            data = Some(resp.json().await?);
        }
        data.unwrap()
    };

    let latest_version = release["tag_name"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches('v')
        .to_owned();
    let current = env!("RSCLAW_BUILD_VERSION");

    kv("Current:", current);
    kv("Latest:", &latest_version);

    if latest_version.is_empty() {
        println!("  {} could not determine latest version", yellow("[!]"));
        return Ok(());
    }

    if latest_version == current {
        println!("  {} already up to date", green("[ok]"));
        return Ok(());
    }

    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "currentVersion": current,
                "latestVersion": latest_version,
                "updateAvailable": true,
                "dryRun": args.dry_run,
            })
        );
        if args.dry_run {
            return Ok(());
        }
    }

    if args.dry_run {
        println!(
            "  {} would update to {latest_version} (dry run)",
            dim("[..]")
        );
        return Ok(());
    }

    // 2. Determine platform asset name candidates (try gnu first, then musl)
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let candidates: Vec<&str> = match (os, arch) {
        ("macos", "aarch64") => vec!["rsclaw-aarch64-apple-darwin"],
        ("macos", "x86_64") => vec!["rsclaw-x86_64-apple-darwin"],
        ("linux", "x86_64") => vec![
            "rsclaw-x86_64-unknown-linux-gnu",
            "rsclaw-x86_64-unknown-linux-musl",
        ],
        ("linux", "aarch64") => vec![
            "rsclaw-aarch64-unknown-linux-gnu",
            "rsclaw-aarch64-unknown-linux-musl",
        ],
        ("windows", "x86_64") => vec!["rsclaw-x86_64-pc-windows-msvc"],
        ("windows", "aarch64") => vec!["rsclaw-aarch64-pc-windows-msvc"],
        _ => anyhow::bail!("unsupported platform: {os}-{arch}"),
    };

    // 3. Find matching asset URL (both sources use GitHub releases format)
    let assets = release["assets"].as_array();
    let mut asset_name = candidates[0];
    let download_url = assets.and_then(|arr| {
        for candidate in &candidates {
            if let Some(url) = arr.iter().find_map(|a| {
                let name = a["name"].as_str().unwrap_or("");
                if name.contains(candidate) {
                    a["browser_download_url"].as_str().map(|s| s.to_owned())
                } else {
                    None
                }
            }) {
                asset_name = candidate;
                return Some(url);
            }
        }
        None
    });

    let Some(url) = download_url else {
        // No pre-built binary, suggest building from source
        println!("  {} no pre-built binary for {os}-{arch}", yellow("[!]"));
        println!("  Update from source:");
        println!("    cd /path/to/rsclaw && git pull && cargo build --release");
        return Ok(());
    };

    // 4. Download binary
    println!("  {} downloading {asset_name}...", dim("[..]"));
    let download = if url.contains("github.com") || url.contains("githubusercontent.com") {
        proxy_url(&url)
    } else {
        url.clone()
    };
    let binary = client.get(&download).send().await?.bytes().await?;

    if binary.is_empty() {
        anyhow::bail!("downloaded binary is empty");
    }

    // 5. Replace current executable
    let current_exe = std::env::current_exe()?;
    let backup = current_exe.with_extension("old");

    // Backup current binary
    std::fs::rename(&current_exe, &backup)?;

    // Write new binary
    std::fs::write(&current_exe, &binary)?;

    // Set executable permission on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&current_exe, std::fs::Permissions::from_mode(0o755))?;
    }

    println!("  {} updated to {latest_version}", green("[ok]"));
    kv("Binary:", &current_exe.display().to_string());
    kv("Backup:", &backup.display().to_string());

    // 6. Restart gateway if running
    if !args.no_restart {
        let pid_file = config::loader::pid_file();
        if pid_file.exists() {
            println!("  {} restarting gateway...", dim("[..]"));
            if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    let _ = crate::sys::process_terminate(pid as u32);
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let _ = std::process::Command::new(&current_exe)
                        .arg("gateway")
                        .arg("start")
                        .spawn();
                    println!("  {} gateway restarted", green("[ok]"));
                }
            }
        }
    }

    println!();
    Ok(())
}

async fn update_status() -> Result<()> {
    banner(&format!(
        "rsclaw update status v{}",
        env!("RSCLAW_BUILD_VERSION")
    ));

    let client = build_update_client(10)?;

    kv("Current:", env!("RSCLAW_BUILD_VERSION"));

    match client.get(&proxy_url(RELEASE_URL)).send().await {
        Ok(resp) if resp.status().is_success() => {
            let release: serde_json::Value = resp.json().await?;
            let latest = release["tag_name"]
                .as_str()
                .unwrap_or("unknown")
                .trim_start_matches('v');
            kv("Latest:", latest);
            if latest == env!("RSCLAW_BUILD_VERSION") {
                println!("  {} up to date", green("[ok]"));
            } else {
                println!("  {} update available: {latest}", yellow("[!]"));
                println!("  Run: rsclaw update");
            }
        }
        _ => {
            println!("  {} could not check for updates", yellow("[!]"));
        }
    }
    println!();
    Ok(())
}
