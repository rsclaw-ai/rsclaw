use anyhow::Result;

use super::style::*;
use crate::{
    cli::{UpdateArgs, UpdateCommand},
    config,
};

pub async fn cmd_update(sub: UpdateCommand) -> Result<()> {
    match sub {
        UpdateCommand::Run(args) => do_update(&args).await?,
        UpdateCommand::Status => update_status().await?,
        UpdateCommand::Wizard => {
            banner(&format!(
                "rsclaw update wizard v{}",
                option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
            ));
            warn_msg("update wizard: not yet implemented");
            println!("  {}", dim("use `rsclaw update run` for now"));
        }
    }
    Ok(())
}

const RSCLAW_VERSION_URL: &str = "https://app.rsclaw.ai/api/version";

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
        .user_agent("rsclaw/dev")
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()?)
}

async fn do_update(args: &UpdateArgs) -> Result<()> {
    banner(&format!(
        "rsclaw update v{}",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));

    let timeout_secs = args.timeout.unwrap_or(30);
    let client = build_update_client(timeout_secs)?;

    // 1. Check latest release — try app.rsclaw.ai first, fallback GitHub
    println!("  {} checking for updates...", dim("[..]"));

    // Try app.rsclaw.ai first (object or array), fallback GitHub releases list (array).
    // A valid release must have tag_name and assets so we can download the binary.
    let release: serde_json::Value = {
        let mut data = None;
        let sources = [
            RSCLAW_VERSION_URL.to_owned(),
            proxy_url(&format!("https://api.github.com/repos/{}/releases?per_page=10", "rsclaw-ai/rsclaw")),
        ];
        for url in &sources {
            if let Ok(resp) = client.get(url).send().await {
                if resp.status().is_success() {
                    let body = resp.bytes().await.unwrap_or_default();
                    if let Some(found) = parse_release_body(&body) {
                        // Only accept if it has assets; otherwise try next source.
                        if found["assets"].is_array() {
                            data = Some(found);
                            break;
                        }
                    }
                }
            }
        }
        data.unwrap_or_default()
    };

    let latest_version = release["tag_name"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches('v')
        .to_owned();
    let current = option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev");

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
        ("macos", "aarch64") => vec!["aarch64-apple-darwin"],
        ("macos", "x86_64") => vec!["x86_64-apple-darwin"],
        ("linux", "x86_64") => vec!["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"],
        ("linux", "aarch64") => vec!["aarch64-unknown-linux-gnu", "aarch64-unknown-linux-musl"],
        ("windows", "x86_64") => vec!["x86_64-pc-windows-msvc"],
        ("windows", "aarch64") => vec!["aarch64-pc-windows-msvc"],
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
    let download = if url.contains("github.com") || url.contains("githubusercontent.com") { proxy_url(&url) } else { url.clone() };
    let downloaded = client.get(&download).send().await?.bytes().await?;

    if downloaded.is_empty() {
        anyhow::bail!("downloaded binary is empty");
    }

    // 5. Extract binary from archive (tar.gz / zip) if needed
    let binary_name = if std::env::consts::OS == "windows" { "rsclaw.exe" } else { "rsclaw" };
    let binary = extract_binary(&downloaded, binary_name, &download)
        .unwrap_or_else(|e| {
            println!("  {} could not extract archive ({}), using raw download", yellow("[!]"), e);
            downloaded.to_vec()
        });

    // 6. Replace current executable
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
        std::fs::set_permissions(
            &current_exe,
            std::fs::Permissions::from_mode(0o755),
        )?;
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

/// Parse a release response body that may be an array or a single object.
/// Returns the first CLI release (tag starts with 'v' but not 'app-v').
fn parse_release_body(body: &[u8]) -> Option<serde_json::Value> {
    // Try array first (GitHub /releases endpoint)
    if let Ok(arr) = serde_json::from_slice::<Vec<serde_json::Value>>(body) {
        return arr.into_iter().find(|r| {
            r["tag_name"]
                .as_str()
                .is_some_and(|t| t.starts_with('v') && !t.starts_with("app-"))
        });
    }
    // Fall back to single object (app.rsclaw.ai/api/version)
    if let Ok(obj) = serde_json::from_slice::<serde_json::Value>(body) {
        if obj["tag_name"]
            .as_str()
            .is_some_and(|t| t.starts_with('v') && !t.starts_with("app-"))
        {
            return Some(obj);
        }
    }
    None
}

/// Extract a named file from a downloaded archive (tar.gz or zip).
/// If the data does not appear to be an archive, returns an error so
/// the caller can fall back to treating it as a raw binary.
fn extract_binary(data: &[u8], filename: &str, url: &str) -> Result<Vec<u8>> {
    let url_lower = url.to_lowercase();
    if url_lower.ends_with(".tar.gz") || url_lower.ends_with(".tgz") {
        let tar = flate2::read::GzDecoder::new(data);
        let mut archive = tar::Archive::new(tar);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;
            if path.file_name().map(|n| n == filename).unwrap_or(false) {
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut buf)?;
                return Ok(buf);
            }
        }
        anyhow::bail!("{filename} not found in tar.gz archive");
    }

    if url_lower.ends_with(".zip") {
        let reader = std::io::Cursor::new(data);
        let mut archive = zip::ZipArchive::new(reader)?;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let name = file.name();
            if std::path::Path::new(name)
                .file_name()
                .map(|n| n == filename)
                .unwrap_or(false)
            {
                let mut buf = Vec::new();
                std::io::copy(&mut file, &mut buf)?;
                return Ok(buf);
            }
        }
        anyhow::bail!("{filename} not found in zip archive");
    }

    anyhow::bail!("unsupported archive format: {url}");
}

async fn update_status() -> Result<()> {
    banner(&format!(
        "rsclaw update status v{}",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));

    let client = build_update_client(10)?;

    kv("Current:", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"));

    // Try app.rsclaw.ai first (object or array), fallback GitHub releases list (array)
    let mut latest_tag: Option<String> = None;
    let sources = [
        RSCLAW_VERSION_URL.to_owned(),
        proxy_url(&format!("https://api.github.com/repos/{}/releases?per_page=10", "rsclaw-ai/rsclaw")),
    ];
    for url in &sources {
        if let Ok(resp) = client.get(url).send().await {
            if resp.status().is_success() {
                let body = resp.bytes().await.unwrap_or_default();
                if let Some(release) = parse_release_body(&body) {
                    if let Some(tag) = release["tag_name"].as_str() {
                        if tag.starts_with('v') && !tag.starts_with("app-") {
                            latest_tag = Some(tag.to_owned());
                            break;
                        }
                    }
                }
            }
        }
    }

    match latest_tag {
        Some(tag) => {
            let latest = tag.trim_start_matches('v');
            kv("Latest:", latest);
            if latest == option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev") {
                println!("  {} up to date", green("[ok]"));
            } else {
                println!("  {} update available: {latest}", yellow("[!]"));
                println!("  Run: rsclaw update");
            }
        }
        None => {
            println!("  {} could not check for updates", yellow("[!]"));
        }
    }
    println!();
    Ok(())
}
