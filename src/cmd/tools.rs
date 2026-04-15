use anyhow::{bail, Result};
use std::path::PathBuf;

use super::style::*;
use crate::cli::ToolsCommand;

// ---------------------------------------------------------------------------
// Mirror URL (Chinese users) / upstream fallback
// ---------------------------------------------------------------------------

const MIRROR_BASE: &str = "https://gitfast.org/tools";

/// Manifest endpoint — returns JSON with versions and download URLs.
const MANIFEST_URL: &str = "https://gitfast.org/tools/manifest.json";

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

struct ToolDef {
    name: &'static str,
    display: &'static str,
    detect_cmd: &'static [&'static str],
    local_bin: &'static str, // relative to tools_dir(), e.g. "chromium/chrome"
}

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "chrome",
        display: "Chrome for Testing (browser automation)",
        detect_cmd: &["google-chrome", "chromium", "chromium-browser", "chrome"],
        local_bin: "chrome",
    },
    ToolDef {
        name: "ffmpeg",
        display: "ffmpeg (audio/video processing)",
        detect_cmd: &["ffmpeg"],
        local_bin: "ffmpeg",
    },
    ToolDef {
        name: "node",
        display: "Node.js (plugin runtime)",
        detect_cmd: &["node"],
        local_bin: "node",
    },
    ToolDef {
        name: "python",
        display: "Python 3 (skill/plugin runtime)",
        detect_cmd: &["python3", "python"],
        local_bin: "python",
    },
    ToolDef {
        name: "sherpa-onnx",
        display: "sherpa-onnx (STT + TTS engine)",
        detect_cmd: &["sherpa-onnx-offline-tts", "sherpa-onnx-offline", "sherpa-onnx"],
        local_bin: "sherpa-onnx",
    },
    ToolDef {
        name: "opencode",
        display: "OpenCode (AI coding agent)",
        detect_cmd: &["opencode"],
        local_bin: "opencode",
    },
    ToolDef {
        name: "claude-code",
        display: "Claude Code (AI coding agent)",
        detect_cmd: &["claude"],
        local_bin: "claude-code",
    },
];

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn tools_dir() -> PathBuf {
    crate::config::loader::base_dir().join("tools")
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

fn is_tool_in_path(def: &ToolDef) -> bool {
    for cmd in def.detect_cmd {
        if which::which(cmd).is_ok() {
            return true;
        }
    }
    false
}

fn is_tool_installed_locally(def: &ToolDef) -> bool {
    let dir = tools_dir().join(def.local_bin);
    dir.exists()
}

fn tool_status(def: &ToolDef) -> &'static str {
    if is_tool_installed_locally(def) {
        "installed"
    } else if is_tool_in_path(def) {
        "system"
    } else {
        "missing"
    }
}

// ---------------------------------------------------------------------------
// Public: tools summary for `rsclaw status`
// ---------------------------------------------------------------------------

/// Returns a one-line tools summary, e.g. "chromium ✓  ffmpeg ✓  node ✓  python ✓  sherpa-onnx ✗"
pub fn tools_summary_line() -> String {
    TOOLS
        .iter()
        .map(|def| {
            let icon = if tool_status(def) == "missing" { "✗" } else { "✓" };
            format!("{} {}", def.name, icon)
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Returns count of (available, total) tools
pub fn tools_count() -> (usize, usize) {
    let available = TOOLS.iter().filter(|d| tool_status(d) != "missing").count();
    (available, TOOLS.len())
}

/// Returns names of missing tools
pub fn tools_missing() -> Vec<&'static str> {
    TOOLS.iter().filter(|d| tool_status(d) == "missing").map(|d| d.name).collect()
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub async fn cmd_tools(sub: ToolsCommand) -> Result<()> {
    match sub {
        ToolsCommand::List => { cmd_list(); Ok(()) }
        ToolsCommand::Status => { cmd_status(); Ok(()) }
        ToolsCommand::Install { name, force } => cmd_install(&name, force).await,
    }
}

fn cmd_list() {
    banner(&format!(
        "rsclaw tools v{}",
        env!("RSCLAW_BUILD_VERSION")
    ));
    println!();

    let dir = tools_dir();
    let mut found = false;

    for def in TOOLS {
        let local_dir = dir.join(def.local_bin);
        if local_dir.exists() {
            println!("  {}  {}", green("✓"), bold(def.name));
            println!("    {}", dim(&local_dir.display().to_string()));
            found = true;
        }
    }

    if !found {
        warn_msg("no tools installed locally");
        println!();
        println!("  Run: rsclaw tools install <name>");
        println!("  Available: chromium, ffmpeg, whisper-cpp, node, all");
    }
}

fn cmd_status() {
    banner(&format!(
        "rsclaw tools v{}",
        env!("RSCLAW_BUILD_VERSION")
    ));
    println!();

    for def in TOOLS {
        let status = tool_status(def);
        let (icon, label) = match status {
            "system" => (green("✓"), green("system PATH")),
            "installed" => (green("✓"), cyan("~/.rsclaw/tools")),
            _ => (red("✗"), red("not found")),
        };
        println!("  {} {:<14} {}  {}", icon, bold(def.name), label, dim(def.display));
    }

    // Check if any missing
    let missing: Vec<_> = TOOLS.iter().filter(|d| tool_status(d) == "missing").collect();
    if !missing.is_empty() {
        println!();
        println!(
            "  Install missing tools: {} or download from {}",
            bold("rsclaw tools install <name>"),
            cyan("https://gitfast.io"),
        );
    }
}

/// Resolve tool name aliases (e.g. "chromium" → "chrome").
/// Find node binary: prefer locally installed, fallback to system PATH.
fn find_node_binary(tools_dir: &std::path::Path) -> Option<String> {
    // Check local tools dir first.
    let local = tools_dir.join("node").join("bin").join("node");
    if local.exists() { return Some(local.to_string_lossy().to_string()); }
    // Windows variant.
    let local_win = tools_dir.join("node").join("node.exe");
    if local_win.exists() { return Some(local_win.to_string_lossy().to_string()); }
    // System PATH.
    which::which("node").ok().map(|p| p.to_string_lossy().to_string())
}

fn resolve_tool_name(name: &str) -> &str {
    match name {
        "chromium" | "chromium-browser" | "google-chrome" => "chrome",
        "python3" => "python",
        "nodejs" | "node.js" => "node",
        "open-code" | "opencode-cli" => "opencode",
        "claude" | "claude-agent" | "claudecode" => "claude-code",
        _ => name,
    }
}

async fn cmd_install(name: &str, force: bool) -> Result<()> {
    let name = resolve_tool_name(name);
    let names: Vec<&str> = if name == "all" {
        TOOLS.iter().map(|d| d.name).collect()
    } else {
        // Validate name
        if !TOOLS.iter().any(|d| d.name == name) {
            bail!(
                "Unknown tool: {name}. Available: {}",
                TOOLS
                    .iter()
                    .map(|d| d.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        vec![name]
    };

    // Fetch manifest from mirror
    println!("Fetching tool manifest from {} ...", dim(MANIFEST_URL));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let manifest: serde_json::Value = match client.get(MANIFEST_URL).send().await {
        Ok(resp) if resp.status().is_success() => resp.json().await?,
        Ok(resp) => bail!("manifest fetch failed: HTTP {}", resp.status()),
        Err(e) => {
            err_msg(&format!("Cannot reach mirror: {e}"));
            println!();
            println!("  Please download manually from: {}", bold("https://gitfast.io"));
            println!("  Then extract to: {}", bold(&tools_dir().display().to_string()));
            return Ok(());
        }
    };

    let dir = tools_dir();
    std::fs::create_dir_all(&dir)?;

    let platform = detect_platform();
    println!("Platform: {}", bold(platform));
    println!();

    for tool_name in &names {
        let def = TOOLS.iter().find(|d| d.name == *tool_name).unwrap();

        // Skip if already available (unless --force)
        if !force && is_tool_in_path(def) {
            println!("  {} {} {}", green("✓"), bold(def.name), dim("(already in system PATH, skipping)"));
            continue;
        }
        if !force && is_tool_installed_locally(def) {
            println!("  {} {} {}", green("✓"), bold(def.name), dim("(already installed, skipping)"));
            continue;
        }

        // npm-based tools: install via npm --prefix instead of downloading binary.
        let npm_package = match *tool_name {
            "claude-code" => Some("@anthropic-ai/claude-code"),
            _ => None,
        };
        if let Some(pkg) = npm_package {
            let dest_dir = dir.join(def.local_bin);
            std::fs::create_dir_all(&dest_dir)?;
            println!("  Installing {} via npm ...", bold(def.name));
            let node_bin = find_node_binary(&dir);
            let npm_bin = node_bin.as_deref().map(|n| {
                let p = std::path::Path::new(n).parent().unwrap_or(std::path::Path::new(""));
                p.join("npm").to_string_lossy().to_string()
            }).unwrap_or_else(|| "npm".to_owned());
            let status = std::process::Command::new(&npm_bin)
                .args(["install", "--prefix", &dest_dir.to_string_lossy(), pkg])
                .status();
            match status {
                Ok(s) if s.success() => ok(&format!("{} installed to {}", def.name, dest_dir.display())),
                Ok(s) => err_msg(&format!("{}: npm install exited with {s}", def.name)),
                Err(e) => {
                    err_msg(&format!("{}: npm not found ({e}). Install node first: rsclaw tools install node", def.name));
                }
            }
            continue;
        }

        let download_url = resolve_download_url(&manifest, tool_name, platform);
        let Some(url) = download_url else {
            warn_msg(&format!(
                "{}: no download available for platform {platform}. Download from https://gitfast.io",
                def.name
            ));
            continue;
        };

        println!("  Installing {} ...", bold(def.name));
        println!("    {}", dim(&url));

        let dest_dir = dir.join(def.local_bin);
        std::fs::create_dir_all(&dest_dir)?;

        match download_and_extract(&client, &url, &dest_dir).await {
            Ok(()) => {
                ok(&format!("{} installed to {}", def.name, dest_dir.display()));
            }
            Err(e) => {
                err_msg(&format!("{}: {e}", def.name));
                println!("    Download manually from: {}", bold("https://gitfast.io"));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Platform detection
// ---------------------------------------------------------------------------

fn detect_platform() -> &'static str {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("macos", "aarch64") => "mac-arm64",
        ("windows", "x86_64") => "win-x64",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Resolve download URL from manifest
// ---------------------------------------------------------------------------

fn resolve_download_url(
    manifest: &serde_json::Value,
    tool: &str,
    platform: &str,
) -> Option<String> {
    // Try manifest.{tool}.downloads.{platform}
    if let Some(url) = manifest
        .get(tool)
        .and_then(|t| t.get("downloads"))
        .and_then(|d| d.get(platform))
        .and_then(|v| v.as_str())
    {
        return Some(url.to_owned());
    }

    // Fallback: construct URL from mirror base + tool conventions
    // manifest keys use underscores (sherpa_onnx), tool names use hyphens (sherpa-onnx)
    let manifest_key = tool.replace('-', "_");
    let section = manifest.get(&manifest_key).or_else(|| manifest.get(tool))?;

    match tool {
        "chrome" | "chromium" => {
            let ver = section.get("version")?.as_str()?;
            let filename = match platform {
                "linux-x64" => "chrome-linux64.zip",
                "mac-x64" => "chrome-mac-x64.zip",
                "mac-arm64" => "chrome-mac-arm64.zip",
                "win-x64" => "chrome-win64.zip",
                _ => return None,
            };
            Some(format!("{MIRROR_BASE}/chrome/{ver}/{filename}"))
        }
        "ffmpeg" => {
            let filename = match platform {
                "linux-x64" => "ffmpeg-linux-x64.tar.xz",
                "linux-arm64" => "ffmpeg-linux-arm64.tar.xz",
                "win-x64" => "ffmpeg-win-x64.zip",
                "mac-x64" | "mac-arm64" => "ffmpeg-mac-x64.zip",
                _ => return None,
            };
            Some(format!("{MIRROR_BASE}/ffmpeg/{filename}"))
        }
        "node" => {
            let ver = section.get("version")?.as_str()?;
            let filename = match platform {
                "linux-x64" => format!("node-linux-x64.tar.xz"),
                "linux-arm64" => format!("node-linux-arm64.tar.xz"),
                "mac-x64" => format!("node-mac-x64.tar.gz"),
                "mac-arm64" => format!("node-mac-arm64.tar.gz"),
                "win-x64" => format!("node-win-x64.zip"),
                _ => return None,
            };
            Some(format!("{MIRROR_BASE}/node/{ver}/{filename}"))
        }
        "python" => {
            let ver = section.get("version")?.as_str()?;
            let filename = match platform {
                "linux-x64" => "python-linux-x64.tar.gz",
                "linux-arm64" => "python-linux-arm64.tar.gz",
                "mac-x64" => "python-mac-x64.tar.gz",
                "mac-arm64" => "python-mac-arm64.tar.gz",
                "win-x64" => "python-win-x64.tar.gz",
                _ => return None,
            };
            Some(format!("{MIRROR_BASE}/python/{ver}/{filename}"))
        }
        "sherpa-onnx" => {
            let ver = section.get("version")?.as_str()?;
            let filename = match platform {
                "linux-x64" => format!("sherpa-onnx-v{ver}-linux-x64-shared-lib.tar.bz2"),
                "linux-arm64" => format!("sherpa-onnx-v{ver}-linux-aarch64-shared-cpu-lib.tar.bz2"),
                "mac-x64" => format!("sherpa-onnx-v{ver}-osx-x64-shared-lib.tar.bz2"),
                "mac-arm64" => format!("sherpa-onnx-v{ver}-osx-arm64-shared-lib.tar.bz2"),
                "win-x64" => format!("sherpa-onnx-v{ver}-win-x64-shared-MT-Release-lib.tar.bz2"),
                _ => return None,
            };
            Some(format!("{MIRROR_BASE}/sherpa-onnx/{ver}/{filename}"))
        }
        "opencode" => {
            let ver = section.get("version")?.as_str()?;
            let filename = match platform {
                "linux-x64" => format!("opencode-linux-x64.tar.gz"),
                "linux-arm64" => format!("opencode-linux-arm64.tar.gz"),
                "mac-x64" => format!("opencode-darwin-x64.zip"),
                "mac-arm64" => format!("opencode-darwin-arm64.zip"),
                "win-x64" => format!("opencode-windows-x64.zip"),
                _ => return None,
            };
            Some(format!("{MIRROR_BASE}/opencode/{ver}/{filename}"))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Download and extract archive
// ---------------------------------------------------------------------------

async fn download_and_extract(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await?
        .error_for_status()?;

    // Stream to temp file to avoid loading entire archive into memory
    // (critical for 1-core/1GB machines where 500MB+ archives would OOM)
    let tmp_dir = tempfile::tempdir()?;
    let filename = url.rsplit('/').next().unwrap_or("download");
    let tmp_path = tmp_dir.path().join(filename);

    {
        let mut stream = resp.bytes_stream();
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        let mut downloaded: u64 = 0;
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;
            // Progress every ~10MB
            if downloaded % (10 * 1024 * 1024) < chunk.len() as u64 {
                print!("\r    Downloaded {}MB...", downloaded / 1_000_000);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
        file.flush().await?;
        println!("\r    Downloaded {}MB, extracting...", downloaded / 1_000_000);
    }

    if url.ends_with(".zip") {
        extract_zip(&tmp_path, dest)?;
    } else if url.ends_with(".tar.xz") {
        extract_tar_xz(&tmp_path, dest)?;
    } else if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        extract_tar_gz(&tmp_path, dest)?;
    } else if url.ends_with(".tar.bz2") {
        extract_tar_bz2(&tmp_path, dest)?;
    } else {
        // Unknown format — move the raw file
        std::fs::rename(&tmp_path, dest.join(filename))?;
    }

    Ok(())
}

fn extract_zip(archive_path: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let Some(name) = file.enclosed_name().map(|n| n.to_owned()) else {
            continue;
        };

        // Strip the top-level directory (e.g. "chrome-linux64/chrome" → "chrome")
        let components: Vec<_> = name.components().collect();
        let rel_path = if components.len() > 1 {
            components[1..].iter().collect::<PathBuf>()
        } else {
            name.clone()
        };

        let out_path = dest.join(&rel_path);

        if file.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Stream extract: copy file-by-file instead of reading all into memory
            let mut out_file = std::fs::File::create(&out_path)?;
            std::io::copy(&mut file, &mut out_file)?;

            // Preserve executable permission on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = file.unix_mode() {
                    std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))?;
                }
            }
        }
    }
    Ok(())
}

fn extract_tar_xz(archive_path: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let buf = std::io::BufReader::new(file);
    let xz_reader = xz2::read::XzDecoder::new(buf);
    extract_tar(xz_reader, dest)
}

fn extract_tar_gz(archive_path: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let buf = std::io::BufReader::new(file);
    let gz_reader = flate2::read::GzDecoder::new(buf);
    extract_tar(gz_reader, dest)
}

fn extract_tar_bz2(archive_path: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let buf = std::io::BufReader::new(file);
    let bz2_reader = bzip2::read::BzDecoder::new(buf);
    extract_tar(bz2_reader, dest)
}

fn extract_tar<R: std::io::Read>(reader: R, dest: &std::path::Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_owned();

        // Strip top-level directory
        let components: Vec<_> = path.components().collect();
        let rel_path = if components.len() > 1 {
            components[1..].iter().collect::<PathBuf>()
        } else {
            path.to_path_buf()
        };

        if rel_path.as_os_str().is_empty() {
            continue;
        }

        let out_path = dest.join(&rel_path);

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            entry.unpack(&out_path)?;
        }
    }
    Ok(())
}
