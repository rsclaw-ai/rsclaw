use anyhow::{bail, Result};
use std::path::PathBuf;
use std::time::Duration;

use super::style::*;
use crate::cli::ToolsCommand;

// ---------------------------------------------------------------------------
// Hub endpoints
// ---------------------------------------------------------------------------

/// Tools manifest — schema-2 JSON: `{schema, updated_at, tools:{name:{version,
/// platforms:{platform:{url,sha256}}}}}`. All download metadata is explicit.
const MANIFEST_URL: &str = "https://hub.rsclaw.ai/tools/manifest.json";

/// Signed hub meta — its `sha256.tools` (covered by the ed25519 signature) pins
/// the tools manifest. The install path verifies the manifest against it.
const META_URL: &str = "https://hub.rsclaw.ai/meta.json";

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

struct ToolDef {
    name: &'static str,
    display: &'static str,
    detect_cmd: &'static [&'static str],
    local_bin: &'static str, // relative to tools_dir(), e.g. "chrome/chrome"
    /// Optional tools are still listed by `tools status` but excluded from
    /// the missing/required tally (no warn from `rsclaw doctor`).
    optional: bool,
}

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "chrome",
        display: "Chrome for Testing (browser automation)",
        detect_cmd: &["google-chrome", "chromium", "chromium-browser", "chrome"],
        local_bin: "chrome",
        optional: false,
    },
    ToolDef {
        name: "ffmpeg",
        display: "ffmpeg (audio/video processing)",
        detect_cmd: &["ffmpeg"],
        local_bin: "ffmpeg",
        optional: false,
    },
    ToolDef {
        name: "node",
        display: "Node.js (plugin runtime)",
        detect_cmd: &["node"],
        local_bin: "node",
        optional: false,
    },
    ToolDef {
        // Optional faster JS runtime for plugins with `runtime: "bun"`. node is
        // the default; bun is opt-in (no Windows-ARM64 build upstream).
        name: "bun",
        display: "Bun (fast JS plugin runtime; node alternative)",
        detect_cmd: &["bun"],
        local_bin: "bun",
        optional: true,
    },
    ToolDef {
        name: "python",
        display: "Python 3 (skill/plugin runtime)",
        detect_cmd: &["python3", "python"],
        local_bin: "python",
        optional: false,
    },
    ToolDef {
        name: "sherpa-onnx",
        display: "sherpa-onnx (STT + TTS engine)",
        detect_cmd: &["sherpa-onnx-offline-tts", "sherpa-onnx-offline", "sherpa-onnx"],
        local_bin: "sherpa-onnx",
        optional: false,
    },
    ToolDef {
        name: "opencode",
        display: "OpenCode (AI coding agent)",
        detect_cmd: &["opencode"],
        local_bin: "opencode",
        optional: false,
    },
    ToolDef {
        name: "claude-code",
        display: "Claude Code (AI coding agent, optional)",
        detect_cmd: &["claude"],
        local_bin: "claude-code",
        optional: true,
    },
];

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn tools_dir() -> PathBuf {
    crate::config::loader::base_dir().join("tools")
}

// ---------------------------------------------------------------------------
// Manifest cache + installed-version markers (manifest-driven tools list)
// ---------------------------------------------------------------------------

/// Path of the cached copy of the remote tools manifest.
fn manifest_cache_path(tools_dir: &std::path::Path) -> PathBuf {
    tools_dir.join(".manifest.json")
}

/// Persist a fetched manifest so the available-tools list survives offline.
fn write_manifest_cache(tools_dir: &std::path::Path, manifest: &serde_json::Value) -> Result<()> {
    std::fs::create_dir_all(tools_dir)?;
    std::fs::write(manifest_cache_path(tools_dir), serde_json::to_vec_pretty(manifest)?)?;
    Ok(())
}

/// Load the cached manifest. Returns None when absent or corrupt (fail-open).
fn load_manifest_cache(tools_dir: &std::path::Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(manifest_cache_path(tools_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write the installed-version marker for a tool (tools_dir/<subdir>/.version).
/// `subdir` is the tool's `local_bin` so the marker sits alongside the binary.
fn write_version_marker(tools_dir: &std::path::Path, subdir: &str, version: &str) -> Result<()> {
    let dir = tools_dir.join(subdir);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(".version"), version.trim().as_bytes())?;
    Ok(())
}

/// Read the installed version of a tool, if a marker exists.
fn installed_version(tools_dir: &std::path::Path, subdir: &str) -> Option<String> {
    let s = std::fs::read_to_string(tools_dir.join(subdir).join(".version")).ok()?;
    let s = s.trim();
    if s.is_empty() { None } else { Some(s.to_owned()) }
}

/// Version string for a tool from a manifest, if present.
fn manifest_tool_version(manifest: &serde_json::Value, tool: &str) -> Option<String> {
    manifest.get("tools")?.get(tool)?.get("version")?.as_str().map(|s| s.to_owned())
}

/// All installable tool names: cached-manifest keys ∪ compiled-in baseline.
/// Sorted + de-duped for byte-stable downstream rendering. `None` cache →
/// baseline only (offline / fetch failed).
pub(crate) fn available_tools(cache: Option<&serde_json::Value>) -> Vec<String> {
    let mut names: Vec<String> = TOOLS.iter().map(|d| d.name.to_owned()).collect();
    // schema 2: installable tools live under `.tools` (keyed by name).
    if let Some(obj) = cache
        .and_then(|v| v.get("tools"))
        .and_then(|v| v.as_object())
    {
        names.extend(obj.keys().cloned());
    }
    names.sort();
    names.dedup();
    names
}

/// Installed tools (those whose local_bin exists) paired with their marker
/// version (None when the marker is missing — e.g. installed by an older build).
pub(crate) fn installed_tools(tools_dir: &std::path::Path) -> Vec<(String, Option<String>)> {
    // Marker + presence both key off `local_bin` (the install subdir) so the
    // marker sits alongside the binary. name == local_bin for current tools.
    let mut out: Vec<(String, Option<String>)> = TOOLS
        .iter()
        .filter(|d| tools_dir.join(d.local_bin).exists())
        .map(|d| (d.name.to_owned(), installed_version(tools_dir, d.local_bin)))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// First-startup fetch only runs when there is no cache yet (NOT an
/// auto-update poll — later startups always skip).
fn should_fetch_manifest(tools_dir: &std::path::Path) -> bool {
    !manifest_cache_path(tools_dir).exists()
}

/// One-shot: if no cached manifest exists, GET it once and cache it.
/// Fail-open — any error leaves the cache absent (readers use the baseline).
pub async fn fetch_manifest_if_missing() {
    let dir = tools_dir();
    if !should_fetch_manifest(&dir) {
        return;
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    match client.get(MANIFEST_URL).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(m) = resp.json::<serde_json::Value>().await {
                let _ = write_manifest_cache(&dir, &m);
            }
        }
        _ => { /* fail-open: leave cache absent */ }
    }
}

/// Public accessor for the tools dir (used by the agent prompt/tool builders).
pub fn tools_dir_pub() -> PathBuf {
    tools_dir()
}

/// Public accessor for the cached manifest (used by the agent prompt/tool builders).
pub fn load_manifest_cache_pub(tools_dir: &std::path::Path) -> Option<serde_json::Value> {
    load_manifest_cache(tools_dir)
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

/// Returns a one-line tools summary, e.g. "chrome ✓  ffmpeg ✓  node ✓  python ✓  sherpa-onnx ✗"
/// Optional tools that are missing render as "·" so they don't look like a failure.
pub fn tools_summary_line() -> String {
    TOOLS
        .iter()
        .map(|def| {
            let status = tool_status(def);
            let icon = match (status, def.optional) {
                ("missing", true) => "·",
                ("missing", false) => "✗",
                _ => "✓",
            };
            format!("{} {}", def.name, icon)
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Returns count of (available, required-total) tools — optional tools are
/// excluded from the denominator so a missing optional never trips a warn.
pub fn tools_count() -> (usize, usize) {
    let required = TOOLS.iter().filter(|d| !d.optional);
    let total = required.clone().count();
    let available = required.filter(|d| tool_status(d) != "missing").count();
    (available, total)
}

/// Returns names of missing required tools (optional tools never reported missing).
pub fn tools_missing() -> Vec<&'static str> {
    TOOLS.iter()
        .filter(|d| !d.optional && tool_status(d) == "missing")
        .map(|d| d.name)
        .collect()
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
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
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
        println!("  Available: chrome, ffmpeg, node, python, opencode, claude-code, all");
    }
}

fn cmd_status() {
    banner(&format!(
        "rsclaw tools v{}",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));
    println!();

    for def in TOOLS {
        let status = tool_status(def);
        let (icon, label) = match (status, def.optional) {
            ("system", _) => (green("✓"), green("system PATH")),
            ("installed", _) => (green("✓"), cyan("~/.rsclaw/tools")),
            ("missing", true) => (dim("·"), dim("not installed (optional)")),
            _ => (red("✗"), red("not found")),
        };
        println!("  {} {:<14} {}  {}", icon, bold(def.name), label, dim(def.display));
    }

    // Hint only when REQUIRED tools are missing (optional ones never warn).
    let missing: Vec<&str> = tools_missing();
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

pub async fn cmd_install(name: &str, force: bool) -> Result<()> {
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

    // Fetch as text so we can verify its hash against the signed meta.
    let manifest_txt = match client.get(MANIFEST_URL).send().await {
        Ok(resp) if resp.status().is_success() => resp.text().await?,
        Ok(resp) => bail!("manifest fetch failed: HTTP {}", resp.status()),
        Err(e) => {
            err_msg(&format!("Cannot reach hub: {e}"));
            println!();
            println!("  Please download manually from: {}", bold("https://gitfast.io"));
            println!("  Then extract to: {}", bold(&tools_dir().display().to_string()));
            return Ok(());
        }
    };

    // Trust anchor (fail-closed): fetch the signed meta, verify its ed25519
    // signature against the pinned key, then confirm this manifest matches the
    // signed `sha256.tools`. A compromised hub can't forge the signature.
    {
        let meta_txt = client
            .get(META_URL)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| anyhow::anyhow!("fetch signed meta.json: {e}"))?
            .text()
            .await?;
        let (_, _, tools_sha) = crate::skill::sig::verify_signed_meta(&meta_txt)?;
        use sha2::{Digest, Sha256};
        let got = format!("{:x}", Sha256::digest(manifest_txt.as_bytes()));
        if tools_sha.is_empty() || got != tools_sha {
            bail!("tools manifest sha256 != signed meta.sha256.tools — refusing");
        }
    }
    let manifest: serde_json::Value = serde_json::from_str(&manifest_txt)
        .map_err(|e| anyhow::anyhow!("parse tools manifest: {e}"))?;

    let dir = tools_dir();
    std::fs::create_dir_all(&dir)?;
    // Refresh the local cache so the available-tools list survives offline.
    let _ = write_manifest_cache(&dir, &manifest);

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
            // Windows ships npm as both `npm` (shell wrapper) and `npm.cmd`
            // (Windows batch). The shell wrapper goes through bash and
            // breaks when spawned from a Windows-native subprocess (it
            // emits Unix-style paths like /d/Program Files/...). Use the
            // .cmd form on Windows; other platforms keep the bare name.
            let npm_basename = if cfg!(target_os = "windows") { "npm.cmd" } else { "npm" };
            let npm_bin = node_bin.as_deref().map(|n| {
                let p = std::path::Path::new(n).parent().unwrap_or(std::path::Path::new(""));
                p.join(npm_basename).to_string_lossy().to_string()
            }).unwrap_or_else(|| npm_basename.to_owned());
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

        // Content-pin the download against tools.{tool}.platforms.{platform}.sha256
        // when present (signed via the hub meta). Absent → unverified (legacy).
        let expected_sha = tool_platform(&manifest, def.name, platform)
            .and_then(|p| p.get("sha256"))
            .and_then(|v| v.as_str());

        match download_and_extract(&client, &url, &dest_dir, expected_sha).await {
            Ok(()) => {
                // Record the installed version (marker alongside the binary).
                if let Some(v) = manifest_tool_version(&manifest, def.name) {
                    let _ = write_version_marker(&dir, def.local_bin, &v);
                }
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

/// The `{url, sha256}` entry for a tool+platform in the schema-2 manifest
/// (`tools.<tool>.platforms.<platform>`). All download metadata is explicit in
/// the (signed) manifest — no built-in per-tool URL conventions.
fn tool_platform<'a>(
    manifest: &'a serde_json::Value,
    tool: &str,
    platform: &str,
) -> Option<&'a serde_json::Value> {
    manifest.get("tools")?.get(tool)?.get("platforms")?.get(platform)
}

fn resolve_download_url(
    manifest: &serde_json::Value,
    tool: &str,
    platform: &str,
) -> Option<String> {
    tool_platform(manifest, tool, platform)?
        .get("url")?
        .as_str()
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Download and extract archive
// ---------------------------------------------------------------------------

/// sha256 hex of a file (content-pin verification of downloads).
fn sha256_file_hex(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path)?;
    Ok(format!("{:x}", Sha256::digest(&data)))
}

/// Download an archive and extract to dest. Public so `cmd/models.rs` can reuse.
pub async fn download_and_extract_public(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
) -> Result<()> {
    download_and_extract(client, url, dest, None).await
}

async fn download_and_extract(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
    expected_sha256: Option<&str>,
) -> Result<()> {
    let tmp_dir = tempfile::tempdir()?;
    let filename = url.rsplit('/').next().unwrap_or("download");
    let tmp_path = tmp_dir.path().join(filename);
    download_resumable(client, url, &tmp_path, filename).await?;

    // Content-pin: verify the downloaded archive against the manifest hash
    // BEFORE extracting/running it. Absent hash → skip (back-compat). The tmp
    // dir auto-deletes on drop, so a mismatch leaves nothing on disk.
    if let Some(exp) = expected_sha256 {
        let got = sha256_file_hex(&tmp_path)?;
        if !got.eq_ignore_ascii_case(exp) {
            bail!("sha256 mismatch for {filename}: expected {exp}, got {got} — refusing");
        }
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

/// Resumable streaming HTTP download with a TTY progress bar. The file lives
/// at `out_path` across runs — interrupted transfers continue via the HTTP
/// `Range` header on the next attempt.
///
/// Resume safety: a sidecar `<out_path>.meta` stashes the upstream
/// `(content_length, last_modified || date)` of the version we started
/// downloading. On resume we send `If-Range` plus a manual size check so a
/// CDN that quietly swapped the file mid-transfer can't produce a Frankenstein
/// archive. Mismatch → wipe the partial and restart from byte 0.
///
/// `label` shows in the progress bar prefix (e.g. "BGE model").
///
/// Returns the total bytes resident at `out_path` after the call.
pub async fn download_resumable(
    client: &reqwest::Client,
    url: &str,
    out_path: &std::path::Path,
    label: &str,
) -> Result<u64> {
    use indicatif::{ProgressBar, ProgressStyle};
    use tokio::io::AsyncWriteExt;
    use futures::StreamExt;

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let meta_path = out_path.with_extension(format!(
        "{}meta",
        out_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!("{e}."))
            .unwrap_or_default()
    ));

    let mut already = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    // Stashed (content_length, version_token) from the previous start. We
    // only attempt a resume when the partial bytes AND the meta both exist;
    // a partial without meta is a leftover from a pre-meta version of this
    // function and gets wiped to be safe.
    let stashed: Option<(u64, String)> = if already > 0 {
        std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| s.split_once('\t').map(|(a, b)| (a.to_owned(), b.to_owned())))
            .and_then(|(len_s, tok)| len_s.parse::<u64>().ok().map(|n| (n, tok)))
    } else {
        None
    };
    if already > 0 && stashed.is_none() {
        let _ = std::fs::remove_file(out_path);
        let _ = std::fs::remove_file(&meta_path);
        already = 0;
    }

    // Issue a single GET with conditional Range + If-Range. If-Range tells the
    // server "only partial-content me if the version still matches"; mismatch
    // makes it return the full body (200) which we handle as a clean restart.
    let mut req = client.get(url).timeout(Duration::from_secs(600));
    if let Some((_stashed_len, ref token)) = stashed {
        req = req
            .header(reqwest::header::RANGE, format!("bytes={already}-"))
            .header(reqwest::header::IF_RANGE, token.as_str());
    }
    let resp = req.send().await?;
    let status = resp.status();

    let resume = match status.as_u16() {
        206 => true,
        200 => false, // server ignored Range or If-Range mismatch — restart
        416 => {
            // Already have the full file (or local is larger than remote).
            // Treat as success at current size and drop the meta sidecar.
            let _ = std::fs::remove_file(&meta_path);
            return Ok(already);
        }
        _ => {
            anyhow::bail!(
                "download {url} failed: HTTP {status} {}",
                status.canonical_reason().unwrap_or("")
            );
        }
    };

    // Capture upstream version token + total size BEFORE consuming the body.
    let total_size = if resume {
        resp.headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.rsplit('/').next())
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| resp.content_length().map(|n| already + n))
    } else {
        resp.content_length()
    };
    // Prefer Last-Modified over Date for If-Range — Date is the response
    // generation timestamp (changes on every request), Last-Modified is
    // the file mtime (stable). ETag would be ideal but many CDNs don't set
    // it. Date is the last-resort fallback so we still catch obvious
    // size-shifts even when the server is barebones.
    let version_token: Option<String> = resp
        .headers()
        .get(reqwest::header::ETAG)
        .or_else(|| resp.headers().get(reqwest::header::LAST_MODIFIED))
        .or_else(|| resp.headers().get(reqwest::header::DATE))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Restart conditions, all of which mean the partial we have on disk is
    // unsafe to append to:
    //   1. server's total differs from the size we stashed last time (file
    //      changed upstream)
    //   2. local partial is somehow larger than the upstream total (truncate
    //      / dd / FS quota glitch extended it past the real size)
    let restart_due_to_size_mismatch = resume
        && match (stashed.as_ref().map(|(n, _)| *n), total_size) {
            (Some(stash_total), Some(now_total)) => stash_total != now_total,
            _ => false,
        };
    let restart_due_to_local_overshoot = resume
        && match total_size {
            Some(now_total) => already > now_total,
            None => false,
        };
    let resume = resume && !restart_due_to_size_mismatch && !restart_due_to_local_overshoot;
    if restart_due_to_size_mismatch {
        tracing::warn!(
            url,
            "download_resumable: server total size changed since previous start; restarting from byte 0"
        );
    }
    if restart_due_to_local_overshoot {
        tracing::warn!(
            url,
            already,
            ?total_size,
            "download_resumable: local partial larger than upstream total; restarting from byte 0"
        );
    }

    // Hide the progress bar in non-TTY contexts (daemon, CI, log file
    // redirect) — otherwise indicatif's `\r`-painted updates pollute logs
    // and look like garbled output. Mirrors the QR popup TTY check from
    // commit 839120a.
    let draw_target = if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        indicatif::ProgressDrawTarget::stderr()
    } else {
        indicatif::ProgressDrawTarget::hidden()
    };
    let bar = if let Some(total) = total_size {
        let bar = ProgressBar::with_draw_target(Some(total), draw_target);
        bar.set_style(
            ProgressStyle::with_template(
                "    {prefix:>12} [{bar:30.cyan/blue}] {bytes:>10}/{total_bytes:>10} {bytes_per_sec:>10} ETA {eta:>5}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
        );
        bar.set_prefix(label.to_owned());
        if resume {
            bar.set_position(already);
        }
        bar
    } else {
        let bar = ProgressBar::with_draw_target(None, draw_target);
        bar.set_prefix(label.to_owned());
        bar
    };

    // (Re)create or append. Persist the version token + total length BEFORE
    // any bytes hit disk so a kill-9 mid-transfer leaves a usable resume
    // anchor.
    let mut file = if resume {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(out_path)
            .await?
    } else {
        // Restarting fresh — drop any stale partial and meta.
        let _ = std::fs::remove_file(out_path);
        let _ = std::fs::remove_file(&meta_path);
        tokio::fs::File::create(out_path).await?
    };
    if let (Some(total), Some(token)) = (total_size, version_token.as_ref()) {
        let _ = std::fs::write(&meta_path, format!("{total}\t{token}"));
    }

    let mut written = if resume { already } else { 0 };
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        bar.set_position(written);
    }
    file.flush().await?;
    bar.finish_and_clear();

    // Successful download — meta sidecar served its purpose, drop it.
    let _ = std::fs::remove_file(&meta_path);

    Ok(written)
}

/// Extract a zip archive to `dest`, stripping the top-level directory.
/// Public so the gateway can extract a pre-downloaded archive without
/// re-downloading.
pub fn extract_zip_public(
    archive_path: &std::path::Path,
    dest: &std::path::Path,
) -> Result<()> {
    extract_zip(archive_path, dest)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn manifest_cache_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let m = json!({"bun": {"version": "1.3.14"}});
        write_manifest_cache(tmp.path(), &m).unwrap();
        assert_eq!(load_manifest_cache(tmp.path()), Some(m));
    }

    #[test]
    fn manifest_cache_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load_manifest_cache(tmp.path()), None);
    }

    #[test]
    fn manifest_cache_corrupt_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".manifest.json"), b"{ not json").unwrap();
        assert_eq!(load_manifest_cache(tmp.path()), None);
    }
}
