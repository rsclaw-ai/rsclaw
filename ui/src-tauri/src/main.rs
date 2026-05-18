// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
#[macro_use]
extern crate objc;

mod stream;

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{Emitter, Manager};
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;

/// True when user has manually stopped gateway (close = quit instead of hide).
static GATEWAY_USER_STOPPED: AtomicBool = AtomicBool::new(false);

/// True when app is exiting (Dock quit, Cmd+Q) — don't prevent window close.
static APP_EXITING: AtomicBool = AtomicBool::new(false);

/// Set by SIGTERM signal handler.
static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_sigterm(_sig: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::Relaxed);
}

/// Apply CREATE_NO_WINDOW on Windows to prevent console popups.
#[cfg(target_os = "windows")]
fn hide_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x08000000) // CREATE_NO_WINDOW
}

#[cfg(not(target_os = "windows"))]
fn hide_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd
}

/// Resolve the rsclaw base data dir.
///
/// Mirrors the gateway-side `rsclaw::config::loader::base_dir` priority:
/// `RSCLAW_BASE_DIR` env override > `~/.rsclaw` default. Tilde expansion
/// is supported on the env value so users can set `RSCLAW_BASE_DIR=~/foo`.
fn rsclaw_base_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("RSCLAW_BASE_DIR") {
        if let Some(rest) = p.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    dirs::home_dir().unwrap_or_default().join(".rsclaw")
}

/// Resolve the tray menu language. Order:
///   1. `gateway.language` field in `~/.rsclaw/rsclaw.json5` (the value the
///      user picked during onboarding — keeps the tray consistent with the
///      gateway daemon's locale).
///   2. `LANG` / `LC_ALL` env var prefix (e.g. `zh_CN.UTF-8` → `zh`).
///   3. `"en"` fallback.
///
/// Returns a normalized two-letter code. Only `zh` and `en` are recognized
/// today; anything else collapses to `en` so the menu is never blank.
fn tray_lang() -> &'static str {
    let cfg_path = rsclaw_base_dir().join("rsclaw.json5");
    if let Ok(body) = std::fs::read_to_string(&cfg_path)
        && let Ok(val) = json5::from_str::<serde_json::Value>(&body)
        && let Some(lang) = val
            .pointer("/gateway/language")
            .and_then(|v| v.as_str())
    {
        return resolve_tray_lang(lang);
    }
    let env_lang = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default();
    resolve_tray_lang(&env_lang)
}

/// Map a free-form language string to a tray locale code. Mirrors the
/// gateway's `i18n::resolve_lang` for the subset the tray supports
/// (today: zh + en). The config field stores human-readable names like
/// `"Chinese"` / `"English"` (set by `cmd_onboard`), not 2-letter codes,
/// so prefix-matching alone is not enough.
fn resolve_tray_lang(s: &str) -> &'static str {
    let l = s.to_ascii_lowercase();
    if l.starts_with("zh")
        || l.starts_with("cn")
        || l.contains("chinese")
        || l.contains("\u{4E2D}\u{6587}")
    {
        "zh"
    } else {
        "en"
    }
}

/// Lookup a tray-menu label in the given language. Falls back to English.
fn tray_label(lang: &str, key: &str) -> &'static str {
    match (lang, key) {
        ("zh", "open") => "\u{6253}\u{5F00} RsClaw",
        ("zh", "start_gw") => "\u{542F}\u{52A8}\u{7F51}\u{5173}",
        ("zh", "stop_gw") => "\u{505C}\u{6B62}\u{7F51}\u{5173}",
        ("zh", "status_gw") => "\u{7F51}\u{5173}\u{72B6}\u{6001}",
        ("zh", "quit") => "\u{9000}\u{51FA}",
        (_, "open") => "Open RsClaw",
        (_, "start_gw") => "Start Gateway",
        (_, "stop_gw") => "Stop Gateway",
        (_, "status_gw") => "Gateway Status",
        (_, "quit") => "Quit",
        _ => "",
    }
}

fn run_rsclaw_command(args: &[&str]) -> Result<String, String> {
    // Try sidecar binary next to the executable first, then fall back to PATH.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let sidecar_result = exe_dir.as_ref().and_then(|dir| {
        let sidecar = dir.join(if cfg!(target_os = "windows") { "rsclaw.exe" } else { "rsclaw" });
        eprintln!("[cmd] sidecar path: {} exists={}", sidecar.display(), sidecar.exists());
        if sidecar.exists() {
            hide_window(std::process::Command::new(&sidecar).args(args))
                .output()
                .ok()
        } else {
            None
        }
    });

    let output = match sidecar_result {
        Some(o) => {
            return if o.status.success() {
                Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                Err(format!(
                    "rsclaw {} failed: {}{}",
                    args.join(" "),
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr),
                ))
            };
        }
        None => {
            // Fallback: try "rsclaw" from PATH.
            hide_window(&mut std::process::Command::new("rsclaw"))
                .args(args)
                .output()
                .map_err(|e| format!("Failed to execute rsclaw: {}", e))?
        }
    };

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "rsclaw {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
    }
}

// -- Tauri commands for frontend --

/// Run rsclaw with arbitrary arguments and return combined stdout+stderr.
#[tauri::command]
fn run_rsclaw_cli(args: Vec<String>) -> Result<String, String> {
    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let (stdout, stderr, success) = match exe_dir.as_ref().and_then(|dir| {
        let sidecar = dir.join(if cfg!(target_os = "windows") { "rsclaw.exe" } else { "rsclaw" });
        if sidecar.exists() {
            hide_window(std::process::Command::new(&sidecar).args(&str_args))
                .output()
                .ok()
        } else {
            None
        }
    }) {
        Some(o) => (
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
            o.status.success(),
        ),
        None => {
            let o = hide_window(&mut std::process::Command::new("rsclaw"))
                .args(&str_args)
                .output()
                .map_err(|e| format!("Failed to execute rsclaw: {}", e))?;
            (
                String::from_utf8_lossy(&o.stdout).to_string(),
                String::from_utf8_lossy(&o.stderr).to_string(),
                o.status.success(),
            )
        }
    };

    // Return combined output (doctor writes to both stdout and stderr)
    let combined = format!("{}{}", stdout, stderr);
    if success {
        Ok(combined)
    } else {
        // Still return output even on failure (doctor may report issues as non-zero exit)
        Ok(combined)
    }
}

#[tauri::command]
fn start_gateway() -> Result<String, String> {
    // Gateway runs as a long-lived process — must spawn, not wait for output.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    if let Some(dir) = &exe_dir {
        let sidecar = dir.join(if cfg!(target_os = "windows") { "rsclaw.exe" } else { "rsclaw" });
        if sidecar.exists() {
            hide_window(
                std::process::Command::new(&sidecar)
                    .args(["gateway", "start"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null()),
            )
            .spawn()
            .map_err(|e| format!("Failed to start gateway: {e}"))?;
            return Ok("gateway starting (sidecar)".to_string());
        }
    }
    // Fallback: PATH
    hide_window(
        std::process::Command::new("rsclaw")
            .args(["gateway", "start"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    )
    .spawn()
    .map_err(|e| format!("Failed to start gateway: {e}"))?;
    Ok("gateway starting (PATH)".to_string())
}

#[tauri::command]
fn stop_gateway() -> Result<String, String> {
    run_rsclaw_command(&["gateway", "stop"])
}

#[tauri::command]
fn gateway_status() -> Result<String, String> {
    run_rsclaw_command(&["gateway", "status"])
}

#[tauri::command]
fn get_config_path() -> Result<String, String> {
    Ok(rsclaw_base_dir().to_string_lossy().to_string())
}

/// Wipe on-disk WebKit / Edge / WebView2 caches that survive a
/// `localStorage.clear()` from the JS side.
///
/// Caller flow (settings -> Clear Local Cache):
///   1. JS: `localStorage.clear()` + `sessionStorage.clear()`
///   2. JS: `invoke("clear_webview_cache_dirs")`
///   3. JS: `location.reload()`
///
/// We delete only *cache* and *transient* directories — not LocalStorage,
/// IndexedDB user data, or anything under `~/.rsclaw/`. Some files may be
/// held open by the running webview; failures on individual subpaths are
/// swallowed and the JS-side reload re-creates them.
///
/// Returns the list of paths that were attempted (one per line) so the UI
/// can show the user what was cleared.
#[tauri::command]
fn clear_webview_cache_dirs() -> Result<String, String> {
    let home = dirs::home_dir().ok_or("no home dir")?;
    let identifier = "ai.rsclaw.app";

    // Per-platform list of cache subtrees that are safe to wipe while the
    // app is running. We keep `LocalStorage` and `IndexedDB` since those
    // hold user-visible chat sessions — JS clears them up-front.
    let candidates: Vec<std::path::PathBuf> = if cfg!(target_os = "macos") {
        let webkit = home
            .join("Library")
            .join("WebKit")
            .join(identifier)
            .join("WebsiteData");
        vec![
            webkit.join("ResourceLoadStatistics"),
            webkit.join("EnhancedSecurity"),
            webkit.join("ServiceWorker"),
            webkit.join("CacheStorage"),
            home.join("Library")
                .join("Caches")
                .join(identifier),
        ]
    } else if cfg!(target_os = "windows") {
        // WebView2 user-data directory; Tauri stores it under the app's
        // local appdata. We delete the cache subtrees only.
        let local = home.join("AppData").join("Local").join(identifier);
        let edge = local
            .join("EBWebView")
            .join("Default");
        vec![
            edge.join("Cache"),
            edge.join("Code Cache"),
            edge.join("Service Worker"),
            edge.join("GPUCache"),
        ]
    } else {
        // Linux WebKitGTK: ~/.cache/<identifier>/ is the cache root.
        vec![home.join(".cache").join(identifier)]
    };

    let mut report = Vec::new();
    for path in &candidates {
        if !path.exists() {
            continue;
        }
        let action = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match action {
            Ok(()) => report.push(format!("removed: {}", path.display())),
            Err(e) => report.push(format!("skipped (in use): {} — {e}", path.display())),
        }
    }

    Ok(if report.is_empty() {
        "nothing to clear".to_owned()
    } else {
        report.join("\n")
    })
}

/// Run initial setup: create directories + seed workspace.
#[tauri::command]
fn run_setup() -> Result<String, String> {
    run_rsclaw_command(&["setup", "--non-interactive"])
}

/// Copy the bundled BGE-small-zh model (shipped via tauri.conf.json
/// `bundle.resources`) into `~/.rsclaw/models/bge-small-zh/` so the gateway
/// finds it on its standard search path.
///
/// Atomic install: copy to a per-PID staging dir, atomic-rename when
/// complete, write the `.rsclaw-managed` sentinel matching what the CLI's
/// `ensure_bge_model_present` writes. While copying, leave a `.seeding.tauri`
/// lock file (containing this PID) next to the target. The CLI's
/// `ensure_bge_model_present` checks for that lock and waits a few seconds
/// before falling back to network download — eliminates the race where a
/// fast user clicks Import before the seed finishes.
///
/// Idempotent: skips when the target already has model.safetensors, so a
/// hand-placed or upgraded model is never clobbered.
fn seed_bundled_bge_model<R: tauri::Runtime>(
    handle: &tauri::AppHandle<R>,
) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::Manager;

    let target = rsclaw_base_dir().join("models").join("bge-small-zh");
    if target.join("model.safetensors").exists() {
        return Ok(());
    }

    // Resource layout under bundle: <resource_dir>/resources/bge-small-zh/{...}
    let res_dir = handle
        .path()
        .resource_dir()
        .map_err(|e| format!("resource_dir unavailable: {e}"))?
        .join("resources/bge-small-zh");
    let weights = res_dir.join("model.safetensors");
    if !weights.exists() {
        eprintln!(
            "[setup] no bundled BGE model at {}; gateway will download",
            res_dir.display()
        );
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Lock file: tells the CLI's ensure_bge_model_present "wait, Tauri is
    // installing — don't redundantly download". Lock body is our PID so the
    // CLI can detect a stale lock from a crashed previous Tauri instance.
    let lock_path = target.with_extension("seeding.tauri");
    let pid = std::process::id();
    let _ = std::fs::write(&lock_path, pid.to_string());
    // RAII guard: delete the lock on every exit path including panic/early
    // return. Inline to avoid adding scopeguard as a Tauri-side dep.
    struct LockGuard(std::path::PathBuf);
    impl Drop for LockGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _lock_guard = LockGuard(lock_path.clone());

    let staging = target.with_extension(format!("seeding.tauri.pid{pid}"));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    std::fs::create_dir_all(&staging)?;
    for filename in ["config.json", "tokenizer.json", "model.safetensors"] {
        let src = res_dir.join(filename);
        let dst = staging.join(filename);
        std::fs::copy(&src, &dst).map_err(|e| {
            format!("copy {} -> {}: {e}", src.display(), dst.display())
        })?;
    }
    // Sentinel content matches what the CLI writes — version + bundle
    // marker so a sentinel-aware tool can tell where the install came from.
    let bytes = std::fs::metadata(staging.join("model.safetensors"))
        .map(|m| m.len())
        .unwrap_or(0);
    let sentinel_body = format!(
        "version={ver}\nsource=tauri-bundle\nbytes={bytes}\ninstalled_at_ms={now}\n",
        ver = env!("CARGO_PKG_VERSION"),
        now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let _ = std::fs::write(staging.join(".rsclaw-managed"), sentinel_body);

    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }
    std::fs::rename(&staging, &target).map_err(|e| {
        format!("rename {} -> {}: {e}", staging.display(), target.display())
    })?;
    eprintln!(
        "[setup] seeded bundled BGE model -> {}",
        target.display()
    );
    Ok(())
}

/// Write a file to an agent's workspace directory (~/.rsclaw/workspace-{agentId}/{fileName})
#[tauri::command]
fn write_workspace_file(agent_id: String, file_name: String, content: String) -> Result<String, String> {
    let ws_dir = rsclaw_base_dir().join(format!("workspace-{}", agent_id));
    let _ = std::fs::create_dir_all(&ws_dir);
    let file_path = ws_dir.join(&file_name);
    std::fs::write(&file_path, &content)
        .map_err(|e| format!("write failed: {e}"))?;
    Ok(file_path.to_string_lossy().to_string())
}

/// Read a file from an agent's workspace directory
#[tauri::command]
fn read_workspace_file(agent_id: String, file_name: String) -> Result<String, String> {
    let file_path = rsclaw_base_dir().join(format!("workspace-{}", agent_id)).join(&file_name);
    std::fs::read_to_string(&file_path).map_err(|e| format!("read failed: {e}"))
}

/// Write config file to ~/.rsclaw/rsclaw.json5
/// Validates the content parses as JSON5 before writing.
#[tauri::command]
fn write_config(content: String) -> Result<String, String> {
    // Validate content parses before writing
    if json5::from_str::<serde_json::Value>(&content).is_err() {
        return Err("invalid JSON5 syntax - fix errors before saving".to_string());
    }

    let config_path = rsclaw_base_dir().join("rsclaw.json5");
    // Create dir if needed.
    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&config_path, &content)
        .map_err(|e| format!("write failed: {e}"))?;
    Ok(config_path.to_string_lossy().to_string())
}

/// Read gateway URL and auth token from config file.
#[tauri::command]
fn get_gateway_port() -> Result<serde_json::Value, String> {
    let config_path = rsclaw_base_dir().join("rsclaw.json5");
    if !config_path.exists() {
        return Ok(serde_json::json!({ "url": "http://localhost:18888", "token": "" }));
    }
    let raw = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let val: serde_json::Value = json5::from_str(&raw).unwrap_or(serde_json::json!({}));
    let port = val.pointer("/gateway/port")
        .and_then(|v| v.as_u64())
        .unwrap_or(18888);
    let bind = val.pointer("/gateway/bind")
        .and_then(|v| v.as_str())
        .unwrap_or("loopback");
    let host = match bind {
        "loopback" | "auto" | "all" => "localhost",
        "custom" => val.pointer("/gateway/bindAddress")
            .and_then(|v| v.as_str())
            .unwrap_or("localhost"),
        ip if ip.contains('.') || ip.contains(':') => ip,
        _ => "localhost",
    };
    // Read auth token: gateway.auth.token > env var
    // If missing, auto-generate one and write it to config.
    // Read-only: use token from config or env, never auto-generate.
    // If user wants auth they configure gateway.auth.token themselves.
    let token = val.pointer("/gateway/auth/token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .or_else(|| std::env::var("RSCLAW_AUTH_TOKEN").ok())
        .unwrap_or_default();

    Ok(serde_json::json!({
        "url": format!("http://{}:{}", host, port),
        "token": token,
    }))
}

/// Read channel accounts from config (channels.xxx.accounts keys).
#[tauri::command]
fn get_channel_accounts() -> Result<serde_json::Value, String> {
    let config_path = rsclaw_base_dir().join("rsclaw.json5");
    if !config_path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let val: serde_json::Value = json5::from_str(&raw).unwrap_or(serde_json::json!({}));
    let mut result = serde_json::Map::new();
    if let Some(channels) = val.get("channels").and_then(|v| v.as_object()) {
        for (ch_name, ch_cfg) in channels {
            let mut accounts = Vec::new();
            if let Some(accts) = ch_cfg.get("accounts").and_then(|v| v.as_object()) {
                for key in accts.keys() {
                    accounts.push(serde_json::Value::String(key.clone()));
                }
            }
            if !accounts.is_empty() {
                result.insert(ch_name.clone(), serde_json::Value::Array(accounts));
            }
        }
    }
    Ok(serde_json::Value::Object(result))
}

/// Read the raw config file content.
#[tauri::command]
fn read_config_file() -> Result<String, String> {
    let config_path = rsclaw_base_dir().join("rsclaw.json5");
    if !config_path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&config_path).map_err(|e| e.to_string())
}

/// Check if rsclaw is already set up (config file exists).
#[tauri::command]
fn check_setup() -> Result<bool, String> {
    let config_path = rsclaw_base_dir().join("rsclaw.json5");
    Ok(config_path.exists())
}

/// Get rsclaw version from the sidecar/PATH binary.
#[tauri::command]
fn get_version() -> Result<String, String> {
    run_rsclaw_command(&["--version"])
}

/// HTML payload for the full-screen glow overlay. Inline so we don't
/// need a separate static file or Next.js route — the overlay is a
/// trivial CSS-animation page that doesn't share anything with the
/// main UI bundle.
const GLOW_OVERLAY_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>RsClaw Activity</title><style>
html,body{margin:0;padding:0;width:100vw;height:100vh;background:transparent;overflow:hidden;pointer-events:none;-webkit-user-select:none;user-select:none}
.glow{position:fixed;inset:0;pointer-events:none;background:radial-gradient(ellipse at center, transparent 55%, rgba(255,165,0,0) 65%, rgba(255,140,0,0.18) 80%, rgba(255,100,0,0.42) 100%);box-shadow:inset 0 0 200px 60px rgba(255,140,0,0.5),inset 0 0 80px 25px rgba(255,165,0,0.7);animation:pulse 2.4s ease-in-out infinite}
@keyframes pulse{0%,100%{opacity:0.65}50%{opacity:1}}
</style></head><body><div class="glow"></div></body></html>"#;

/// Open a borderless transparent click-through window covering the
/// primary monitor with a pulsing orange glow on the edges. Used by
/// the `computer_use` overlay so the user gets a screen-wide visual
/// signal that an agent is driving the desktop, not just a glow on
/// the (now-shrunken) main window. Idempotent: re-invoking when the
/// window already exists is a no-op.
#[tauri::command]
async fn open_glow_overlay(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{LogicalPosition, LogicalSize, WebviewUrl, WebviewWindowBuilder};

    if app.get_webview_window("computer-use-glow").is_some() {
        return Ok(());
    }

    let monitor = app
        .primary_monitor()
        .map_err(|e| format!("primary_monitor: {e}"))?
        .ok_or_else(|| "no primary monitor".to_string())?;
    let scale = monitor.scale_factor();
    let size = monitor.size();
    let pos = monitor.position();
    let logical_w = size.width as f64 / scale;
    let logical_h = size.height as f64 / scale;
    let logical_x = pos.x as f64 / scale;
    let logical_y = pos.y as f64 / scale;

    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    let encoded = utf8_percent_encode(GLOW_OVERLAY_HTML, NON_ALPHANUMERIC).to_string();
    let html_url = format!("data:text/html;charset=utf-8,{encoded}");
    let parsed_url: url::Url = html_url
        .parse()
        .map_err(|e: url::ParseError| format!("parse data url: {e}"))?;
    let webview_url = WebviewUrl::External(parsed_url);

    let window = WebviewWindowBuilder::new(&app, "computer-use-glow", webview_url)
        .title("RsClaw Activity Overlay")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .visible(false) // shown after geometry is set so the user doesn't see a flicker
        .inner_size(logical_w, logical_h)
        .position(logical_x, logical_y)
        .build()
        .map_err(|e| format!("build glow window: {e}"))?;

    // Ignore cursor events so the overlay is fully click-through.
    window
        .set_ignore_cursor_events(true)
        .map_err(|e| format!("set_ignore_cursor_events: {e}"))?;
    // Resize/reposition once visible to be sure the geometry stuck.
    window
        .set_size(LogicalSize::new(logical_w, logical_h))
        .map_err(|e| format!("set_size: {e}"))?;
    window
        .set_position(LogicalPosition::new(logical_x, logical_y))
        .map_err(|e| format!("set_position: {e}"))?;
    window.show().map_err(|e| format!("show: {e}"))?;
    Ok(())
}

/// Close the full-screen glow overlay window if open. No-op when the
/// window doesn't exist.
#[tauri::command]
async fn close_glow_overlay(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("computer-use-glow") {
        window
            .close()
            .map_err(|e| format!("close glow window: {e}"))?;
    }
    Ok(())
}

/// Scan OpenClaw data and return summary (agents, sessions, jsonl files).
#[tauri::command]
fn scan_openclaw(path: String) -> Result<serde_json::Value, String> {
    let dir = std::path::PathBuf::from(&path);
    if !dir.is_dir() {
        return Ok(serde_json::json!({ "agents": 0, "sessions": 0, "jsonl": 0 }));
    }
    // Scan agents directory
    let agents_dir = dir.join("agents");
    let mut agent_count = 0usize;
    let mut session_count = 0usize;
    let mut jsonl_count = 0usize;
    if agents_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                    agent_count += 1;
                    let sessions_dir = entry.path().join("sessions");
                    if sessions_dir.is_dir() {
                        if let Ok(sess_entries) = std::fs::read_dir(&sessions_dir) {
                            for se in sess_entries.flatten() {
                                let p = se.path();
                                if se.file_type().map_or(false, |ft| ft.is_dir()) {
                                    session_count += 1;
                                } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                                    // Count files containing .jsonl (e.g. .jsonl, .jsonl.reset.*)
                                    if name.contains(".jsonl") {
                                        session_count += 1;
                                        jsonl_count += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(serde_json::json!({
        "agents": agent_count,
        "sessions": session_count,
        "jsonl": jsonl_count,
    }))
}

/// Detect OpenClaw installation. Returns path if found, null otherwise.
#[tauri::command]
fn detect_openclaw() -> Result<Option<String>, String> {
    // 1. OPENCLAW_CONFIG_PATH -> derive dir from config file.
    if let Ok(val) = std::env::var("OPENCLAW_CONFIG_PATH") {
        let p = std::path::PathBuf::from(&val);
        if p.is_file() {
            if let Some(dir) = p.parent() {
                return Ok(Some(dir.to_string_lossy().to_string()));
            }
        }
    }
    // 2. OPENCLAW_HOME -> direct dir.
    if let Ok(val) = std::env::var("OPENCLAW_HOME") {
        let dir = std::path::PathBuf::from(&val);
        if dir.is_dir()
            && (dir.join("openclaw.json").is_file() || dir.join("agents").is_dir())
        {
            return Ok(Some(val));
        }
    }
    // Check default locations.
    if let Some(home) = dirs::home_dir() {
        for name in &[".openclaw", "bak.openclaw"] {
            let dir = home.join(name);
            if dir.is_dir()
                && (dir.join("openclaw.json").is_file() || dir.join("agents").is_dir())
            {
                return Ok(Some(dir.to_string_lossy().to_string()));
            }
        }
    }
    Ok(None)
}

/// Start channel login (wechat/feishu) as a background process.
/// Returns the temp QR image path to monitor.
#[tauri::command]
fn channel_login_start(channel: String) -> Result<String, String> {
    let qr_path = std::env::temp_dir().join("rsclaw_qr.png");
    // Remove stale QR file so we can detect when a new one appears
    let _ = std::fs::remove_file(&qr_path);

    // Record config mtime for login completion detection
    let config_path = rsclaw_base_dir().join("rsclaw.json5");
    let mtime = std::fs::metadata(&config_path).ok().and_then(|m| m.modified().ok());
    *LOGIN_START_MTIME.lock().unwrap() = mtime.or(Some(std::time::SystemTime::now()));

    // Try sidecar binary next to executable
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let spawned = exe_dir.as_ref().and_then(|dir| {
        let sidecar = dir.join(if cfg!(target_os = "windows") { "rsclaw.exe" } else { "rsclaw" });
        if sidecar.exists() {
            // hide_window prevents a flashing cmd console on Windows.
            hide_window(
                std::process::Command::new(&sidecar)
                    .args(["channels", "login", "--quiet", &channel])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null()),
            )
            .spawn()
            .ok()
        } else {
            None
        }
    });

    if spawned.is_none() {
        // Fallback: spawn via PATH
        hide_window(
            std::process::Command::new("rsclaw")
                .args(["channels", "login", "--quiet", &channel])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
        )
        .spawn()
        .map_err(|e| format!("Failed to start login: {e}"))?;
    }

    Ok(qr_path.to_string_lossy().to_string())
}

/// Track config mtime at login start
static LOGIN_START_MTIME: std::sync::Mutex<Option<std::time::SystemTime>> = std::sync::Mutex::new(None);

/// Check if channel login completed by comparing config mtime.
#[tauri::command]
fn channel_login_status() -> Result<String, String> {
    let qr_path = std::env::temp_dir().join("rsclaw_qr.png");
    let config_path = rsclaw_base_dir().join("rsclaw.json5");

    let start_mtime = LOGIN_START_MTIME.lock().unwrap().clone();
    if let Some(start) = start_mtime {
        // Config was modified after login started = login succeeded
        if let Ok(meta) = std::fs::metadata(&config_path) {
            if let Ok(modified) = meta.modified() {
                if modified > start {
                    *LOGIN_START_MTIME.lock().unwrap() = None;
                    let _ = std::fs::remove_file(&qr_path);
                    return Ok("done".to_string());
                }
            }
        }
    }
    if qr_path.exists() {
        Ok("waiting".to_string())
    } else {
        Ok("idle".to_string())
    }
}

/// Read the temp QR PNG as base64 data URI.
#[tauri::command]
fn channel_login_qr() -> Result<Option<String>, String> {
    let qr_path = std::env::temp_dir().join("rsclaw_qr.png");
    if !qr_path.exists() {
        return Ok(None);
    }
    let data = std::fs::read(&qr_path).map_err(|e| e.to_string())?;
    // Simple base64 encode without extra deps
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::with_capacity(data.len() * 4 / 3 + 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        b64.push(B64[((n >> 18) & 0x3F) as usize] as char);
        b64.push(B64[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 { b64.push(B64[((n >> 6) & 0x3F) as usize] as char); } else { b64.push('='); }
        if chunk.len() > 2 { b64.push(B64[(n & 0x3F) as usize] as char); } else { b64.push('='); }
    }
    Ok(Some(format!("data:image/png;base64,{}", b64)))
}

/// Save cron jobs by calling the gateway's bulk_replace HTTP API.
///
/// **F2 — Tauri must NOT write `cron.json5` directly anymore.** The
/// gateway is the sole writer; cron storage lives in redb and the
/// file is just a best-effort export. Going through the API makes
/// every UI mutation atomic with respect to the cron runner and the
/// ws cron.* methods, eliminating the 3-writer race that caused
/// "I disabled it but it keeps firing".
///
/// Falls back to the legacy direct-file path **only** when the
/// gateway is offline (loopback connect refused). In that case we
/// still write the file so the UI works in "cold edit" mode; the
/// next gateway boot reconciles the file → redb anyway.
#[tauri::command]
fn save_cron_jobs(content: String) -> Result<(), String> {
    let port = get_gateway_port_number();
    let body = content.clone();
    let body_len = body.len();
    let request = format!(
        "PUT /api/v1/cron/bulk_replace HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {body_len}\r\n\
         Connection: close\r\n\r\n{body}"
    );

    use std::io::{Read, Write};
    let addr = format!("127.0.0.1:{port}").parse::<std::net::SocketAddr>()
        .map_err(|e| format!("invalid addr: {e}"))?;
    match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3)) {
        Ok(mut stream) => {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            stream.write_all(request.as_bytes())
                .map_err(|e| format!("send failed: {e}"))?;
            let mut resp = Vec::with_capacity(512);
            let _ = stream.read_to_end(&mut resp);
            // Parse the HTTP status line to surface backend errors.
            let resp_str = String::from_utf8_lossy(&resp);
            if let Some(first_line) = resp_str.lines().next() {
                if !first_line.contains(" 200 ") {
                    // 4xx/5xx: extract the JSON body for a friendlier
                    // toast in the UI. Body is after the empty line.
                    let body_start = resp_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                    let body = &resp_str[body_start..];
                    return Err(format!("gateway rejected: {}", body.trim()));
                }
            }
            Ok(())
        }
        Err(_) => {
            // Gateway is offline — write the file directly so the user
            // can still tweak jobs in cold mode. Next gateway boot
            // imports via reconcile_file_to_redb_on_boot.
            let path = rsclaw_base_dir().join("cron.json5");
            std::fs::write(&path, &content).map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

/// Get gateway port number (default 18888)
fn get_gateway_port_number() -> u64 {
    let config_path = rsclaw_base_dir().join("rsclaw.json5");
    if !config_path.exists() {
        return 18888;
    }
    let raw = std::fs::read_to_string(&config_path).unwrap_or_default();
    let val: serde_json::Value = json5::from_str(&raw).unwrap_or(serde_json::json!({}));
    val.pointer("/gateway/port")
        .and_then(|v| v.as_u64())
        .unwrap_or(18888)
}

/// Ask the running gateway to shut down via a plain HTTP POST. The endpoint
/// is loopback-only on the gateway side (see `is_loopback` in server), so no
/// auth token is needed from the Tauri tray handler.
fn http_shutdown_gateway() -> Result<(), String> {
    use std::io::{Read, Write};
    let port = get_gateway_port_number();
    let addr = format!("127.0.0.1:{port}").parse::<std::net::SocketAddr>()
        .map_err(|e| format!("invalid addr: {e}"))?;
    let mut stream = std::net::TcpStream::connect_timeout(
        &addr, std::time::Duration::from_secs(2),
    ).map_err(|e| format!("connect failed: {e}"))?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let req =
        "POST /api/v1/shutdown HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf); // drain response (best-effort)
    Ok(())
}

/// Read cron jobs by calling the gateway's HTTP API (authoritative
/// redb source). Falls back to reading `cron.json5` when the gateway
/// is offline so the UI still shows something in cold-edit mode.
#[tauri::command]
fn get_cron_jobs() -> Result<serde_json::Value, String> {
    let port = get_gateway_port_number();
    let request = format!(
        "GET /api/v1/cron HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Connection: close\r\n\r\n"
    );

    use std::io::{Read, Write};
    if let Ok(addr) = format!("127.0.0.1:{port}").parse::<std::net::SocketAddr>() {
        if let Ok(mut stream) =
            std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2))
        {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut resp = Vec::with_capacity(8192);
                let _ = stream.read_to_end(&mut resp);
                let resp_str = String::from_utf8_lossy(&resp);
                if resp_str.lines().next().map_or(false, |l| l.contains(" 200 ")) {
                    if let Some(body_start) = resp_str.find("\r\n\r\n") {
                        let body = &resp_str[body_start + 4..];
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
                            return Ok(val);
                        }
                    }
                }
            }
        }
    }

    // Gateway unreachable — fall back to file (cold-edit mode).
    let path = rsclaw_base_dir().join("cron.json5");
    if !path.exists() {
        return Ok(serde_json::json!({ "jobs": [] }));
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let val: serde_json::Value =
        json5::from_str(&raw).unwrap_or(serde_json::json!({ "jobs": [] }));
    Ok(val)
}

/// List installed skills by reading ~/.rsclaw/skills/ directory
#[tauri::command]
fn get_skills() -> Result<serde_json::Value, String> {
    let skills_dir = rsclaw_base_dir().join("skills");
    let mut skills = Vec::new();
    if skills_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Skip hidden dirs and metadata dirs
                    if name.starts_with('.') { continue; }
                    let skill_path = entry.path();
                    // Try to read SKILL.md for metadata
                    let mut description = String::new();
                    let mut version = String::new();
                    let mut author = String::new();
                    let mut tools = Vec::<String>::new();
                    let skill_md = skill_path.join("SKILL.md");
                    if skill_md.exists() {
                        if let Ok(content) = std::fs::read_to_string(&skill_md) {
                            // Parse YAML front-matter
                            if content.starts_with("---") {
                                if let Some(end) = content[3..].find("---") {
                                    let yaml = &content[3..3+end];
                                    for line in yaml.lines() {
                                        let line = line.trim();
                                        if let Some(v) = line.strip_prefix("description:") {
                                            description = v.trim().trim_matches('"').to_string();
                                        } else if let Some(v) = line.strip_prefix("version:") {
                                            version = v.trim().trim_matches('"').to_string();
                                        } else if let Some(v) = line.strip_prefix("author:") {
                                            author = v.trim().trim_matches('"').to_string();
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Count tool scripts
                    if let Ok(dir_entries) = std::fs::read_dir(&skill_path) {
                        for de in dir_entries.flatten() {
                            let n = de.file_name().to_string_lossy().to_string();
                            if (n.ends_with(".sh") || n.ends_with(".py") || n.ends_with(".js")) && !n.starts_with('.') {
                                tools.push(n.rsplit('.').nth(1).unwrap_or(&n).to_string());
                            }
                        }
                    }
                    skills.push(serde_json::json!({
                        "name": name,
                        "description": description,
                        "version": version,
                        "author": author,
                        "tools": tools,
                        "path": skill_path.to_string_lossy(),
                    }));
                }
            }
        }
    }
    skills.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(serde_json::json!({ "skills": skills }))
}

/// Install a skill via sidecar
#[tauri::command]
fn install_skill(name: String) -> Result<String, String> {
    run_rsclaw_command(&["skills", "install", &name])
}

/// Search skills online via sidecar, parse table output
#[tauri::command]
fn search_skills(query: String) -> Result<serde_json::Value, String> {
    let raw = run_rsclaw_command(&["skills", "search", &query])?;
    let mut results = Vec::new();
    for line in raw.lines().skip(1) {
        let stripped = strip_ansi(line);
        let parts: Vec<&str> = stripped.splitn(3, |c: char| c.is_whitespace()).filter(|s| !s.is_empty()).collect();
        if parts.len() >= 2 {
            let name = parts[0].trim();
            let version = parts[1].trim();
            let desc = if parts.len() >= 3 { parts[2].trim() } else { "" };
            if !name.is_empty() && name != "NAME" {
                results.push(serde_json::json!({
                    "name": name,
                    "version": version,
                    "description": desc,
                }));
            }
        }
    }
    Ok(serde_json::json!({ "results": results }))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_esc = false;
    for c in s.chars() {
        if c == '\x1b' { in_esc = true; continue; }
        if in_esc {
            if c.is_ascii_alphabetic() { in_esc = false; }
            continue;
        }
        out.push(c);
    }
    out
}

/// Uninstall a skill via sidecar
#[tauri::command]
fn uninstall_skill(name: String) -> Result<String, String> {
    run_rsclaw_command(&["skills", "uninstall", &name])
}

/// Expand `${VAR}` placeholders in a string by reading from the process
/// environment. Mirrors `crate::config::loader::expand_env_vars` in the
/// gateway so the test path matches actual runtime substitution.
/// Unknown vars are kept as the literal `${VAR}` token.
fn expand_env_vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end_off) = s[i + 2..].find('}') {
                let var_name = &s[i + 2..i + 2 + end_off];
                if !var_name.is_empty()
                    && var_name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    if let Ok(val) = std::env::var(var_name) {
                        out.push_str(&val);
                    } else {
                        out.push_str(&s[i..i + 2 + end_off + 1]);
                    }
                    i += 2 + end_off + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Test provider API key by calling /v1/models directly (no gateway needed).
#[tauri::command]
async fn test_provider(provider: String, api_key: String, base_url: Option<String>, api_type: Option<String>) -> Result<serde_json::Value, String> {
    // Expand `${VAR}` env placeholders so a config like `apiKey: "${ANTHROPIC_API_KEY}"`
    // tests the actual key, matching the gateway's runtime expansion.
    let raw_key = api_key.clone();
    let api_key = expand_env_vars(&api_key);
    let base_url = base_url.map(|u| expand_env_vars(&u));
    // Diagnose the silent failure mode: user has `apiKey: "${FOO}"` in
    // config but `FOO` is unset / empty in the gateway's process env, so
    // expansion produces "" and the test sends an empty header — provider
    // returns 401, user sees "invalid key" and chases the key value when
    // the real problem is the env var.
    let needs_key = matches!(provider.as_str(), "ollama") == false;
    if needs_key && api_key.is_empty() && raw_key.contains("${") {
        let var = raw_key
            .trim()
            .trim_start_matches("${")
            .trim_end_matches('}');
        return Ok(serde_json::json!({
            "ok": false,
            "error": format!(
                "API key expanded to empty — env var '{var}' is unset or empty in the desktop app's process environment. \
                 Either set it before launching RsClaw, or paste the literal key into rsclaw.json5."
            )
        }));
    }
    // Resolve the effective API type. custom/codingplan always pick auth via
    // api_type. doubao opts in too (CodingPlan offering speaks Anthropic),
    // but only when an api_type was explicitly chosen — otherwise it stays
    // "doubao" (auth=bearer, ark base URL).
    let is_custom_like = provider == "custom" || provider == "codingplan";
    let supports_api_type = is_custom_like || provider == "doubao";
    let effective_api_type = if supports_api_type && api_type.is_some() {
        api_type.as_deref().unwrap_or("openai")
    } else {
        provider.as_str()
    };

    // Resolve base URL: explicit base_url param > provider default
    let default_base = match provider.as_str() {
        "anthropic"   => "https://api.anthropic.com/v1",
        "openai"      => "https://api.openai.com/v1",
        "deepseek"    => "https://api.deepseek.com/v1",
        "qwen"        => "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "doubao"      => "https://ark.cn-beijing.volces.com/api/v3",
        "minimax"     => "https://api.minimaxi.com/v1",
        "kimi"        => "https://api.moonshot.cn/v1",
        "zhipu"       => "https://open.bigmodel.cn/api/paas/v4",
        "groq"        => "https://api.groq.com/openai/v1",
        "grok"        => "https://api.x.ai/v1",
        "gemini"      => "https://generativelanguage.googleapis.com/v1beta",
        "siliconflow" => "https://api.siliconflow.cn/v1",
        "openrouter"  => "https://openrouter.ai/api/v1",
        "gaterouter"  => "https://api.gaterouter.ai/openai/v1",
        "ollama"      => "http://localhost:11434",
        "custom" | "codingplan" => "",
        _ => return Ok(serde_json::json!({"ok": false, "error": "unknown provider"})),
    };
    let effective_base = base_url
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| default_base.to_owned());
    let effective_base = effective_base.trim_end_matches('/');

    // Determine auth style based on provider or api_type
    let auth_style = match effective_api_type {
        "anthropic" => "x-api-key",
        "gemini"    => "gemini-key",  // query param auth
        "ollama"    => "none",
        _ => if api_key.is_empty() { "none" } else { "bearer" },
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build().unwrap_or_default();

    // Minimax doesn't support /models — return built-in list
    if provider == "minimax" {
        return Ok(serde_json::json!({
            "ok": true,
            "models": ["MiniMax-M2.7","MiniMax-M2.7-highspeed","MiniMax-M2.5","MiniMax-M2.5-highspeed","MiniMax-M2.1","MiniMax-M2.1-highspeed","MiniMax-M2"]
        }));
    }

    let is_ollama = effective_api_type == "ollama";
    let is_gemini = effective_api_type == "gemini" || provider == "gemini";

    let url = if is_ollama {
        format!("{effective_base}/api/tags")
    } else if is_gemini {
        format!("{effective_base}/models?key={api_key}")
    } else {
        format!("{effective_base}/models")
    };

    let mut req = client.get(&url);
    match auth_style {
        "bearer" => { req = req.header("Authorization", format!("Bearer {api_key}")); }
        "x-api-key" => {
            // Send BOTH headers so the same code path covers:
            //   - Standard Anthropic (api.anthropic.com) — uses x-api-key
            //   - Volcengine "coding plan" Anthropic-compat
            //     (ark.cn-beijing.volces.com/api/coding/v1) — uses Bearer
            // Each backend honors its expected header and ignores the other.
            req = req
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("Authorization", format!("Bearer {api_key}"));
        }
        _ => {} // gemini uses query param, ollama needs no auth
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            // Extract model IDs — handle different response formats
            let models: Vec<String> = if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
                // OpenAI format: { data: [{ id: "..." }] }
                data.iter().filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_owned())).collect()
            } else if let Some(models) = body.get("models").and_then(|m| m.as_array()) {
                // Ollama / Gemini format: { models: [{ name: "..." }] }
                models.iter().filter_map(|m| {
                    m.get("name").or_else(|| m.get("id"))
                        .and_then(|v| v.as_str())
                        // Gemini returns "models/gemini-2.5-flash" — strip prefix
                        .map(|s| s.strip_prefix("models/").unwrap_or(s).to_owned())
                }).collect()
            } else { vec![] };
            Ok(serde_json::json!({"ok": true, "models": models}))
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let msg = if status == 401 || status == 403 {
                "Invalid API key".to_owned()
            } else {
                body[..body.len().min(200)].to_owned()
            };
            Ok(serde_json::json!({"ok": false, "error": msg}))
        }
        Err(e) => Ok(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

/// Run OpenClaw migration.
#[tauri::command]
fn migrate_openclaw(source_path: String) -> Result<String, String> {
    run_rsclaw_command(&["migrate", "--openclaw-dir", &source_path])
}

#[tauri::command]
fn set_auto_start(enable: bool) -> Result<bool, String> {
    let app_name = "RsClaw";
    let raw_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {}", e))?
        .to_string_lossy()
        .to_string();
    // Wrap in quotes for paths with spaces (Windows registry requires this).
    let app_path = if raw_path.contains(' ') && !raw_path.starts_with('"') {
        format!("\"{}\"", raw_path)
    } else {
        raw_path
    };

    let auto = auto_launch::AutoLaunchBuilder::new()
        .set_app_name(app_name)
        .set_app_path(&app_path)
        .set_use_launch_agent(true) // macOS: use LaunchAgent plist
        .build()
        .map_err(|e| format!("Failed to build auto-launch: {}", e))?;

    if enable {
        auto.enable()
            .map_err(|e| format!("Failed to enable auto-start: {}", e))?;
    } else {
        auto.disable()
            .map_err(|e| format!("Failed to disable auto-start: {}", e))?;
    }

    Ok(enable)
}

#[tauri::command]
fn get_auto_start() -> Result<bool, String> {
    let app_name = "RsClaw";
    let raw_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {}", e))?
        .to_string_lossy()
        .to_string();
    // Wrap in quotes for paths with spaces (Windows registry requires this).
    let app_path = if raw_path.contains(' ') && !raw_path.starts_with('"') {
        format!("\"{}\"", raw_path)
    } else {
        raw_path
    };

    let auto = auto_launch::AutoLaunchBuilder::new()
        .set_app_name(app_name)
        .set_app_path(&app_path)
        .set_use_launch_agent(true)
        .build()
        .map_err(|e| format!("Failed to build auto-launch: {}", e))?;

    auto.is_enabled()
        .map_err(|e| format!("Failed to check auto-start status: {}", e))
}

/// Open a file or directory with the system default application.
#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("open failed: {e}"))?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("open failed: {e}"))?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("open failed: {e}"))?;
    }
    Ok(())
}

/// Called by frontend when user manually stops/starts gateway.
#[tauri::command]
fn set_gateway_user_stopped(stopped: bool) {
    GATEWAY_USER_STOPPED.store(stopped, Ordering::Relaxed);
}

/// Save a user-attached image (paste or upload) to
/// `<Downloads>/rsclaw/images/<nanos_hex>/attach.<ext>` and return the
/// absolute path. Keeps base64 blobs out of chat history so typing does
/// not force React to re-diff MB of string per keystroke.
#[tauri::command]
fn save_attach_image(bytes: Vec<u8>, extension: String) -> Result<String, String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let ext = {
        let e = extension.trim_start_matches('.').to_ascii_lowercase();
        if e.is_empty() || !e.chars().all(|c| c.is_ascii_alphanumeric()) {
            "jpg".to_string()
        } else {
            e
        }
    };
    let base = dirs::download_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Downloads"))
        .join("rsclaw")
        .join("images")
        .join(format!("{nanos:x}"));
    std::fs::create_dir_all(&base).map_err(|e| format!("create_dir: {e}"))?;
    let full = base.join(format!("attach.{ext}"));
    std::fs::write(&full, &bytes).map_err(|e| format!("write: {e}"))?;
    Ok(full.to_string_lossy().to_string())
}

/// Read a local image file and return it as a `data:image/...;base64,...`
/// URL. Used at LLM-send time to rehydrate disk-backed attachments into
/// the format upstream APIs expect.
#[tauri::command]
fn read_file_as_data_url(path: String) -> Result<String, String> {
    let p = std::path::Path::new(&path);
    let data = std::fs::read(p).map_err(|e| format!("read: {e}"))?;
    let mime = match p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("heic") => "image/heic",
        Some("heif") => "image/heif",
        _ => "image/jpeg",
    };
    const B64: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::with_capacity(data.len() * 4 / 3 + 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        b64.push(B64[((n >> 18) & 0x3F) as usize] as char);
        b64.push(B64[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            b64.push(B64[((n >> 6) & 0x3F) as usize] as char);
        } else {
            b64.push('=');
        }
        if chunk.len() > 2 {
            b64.push(B64[(n & 0x3F) as usize] as char);
        } else {
            b64.push('=');
        }
    }
    Ok(format!("data:{mime};base64,{b64}"))
}

/// macOS: hide app from dock (agent mode). App only visible via tray icon.
#[cfg(target_os = "macos")]
fn set_dock_visible(visible: bool) {
    unsafe {
        let app: *mut objc::runtime::Object = objc::msg_send![objc::class!(NSApplication), sharedApplication];
        // NSApplicationActivationPolicyRegular = 0 (show in dock)
        // NSApplicationActivationPolicyAccessory = 1 (no dock, no menu bar)
        let policy: i64 = if visible { 0 } else { 1 };
        let _: () = objc::msg_send![app, setActivationPolicy: policy];
    }
}

fn stop_gateway_sync() {
    eprintln!("[shutdown] stopping gateway...");
    match run_rsclaw_command(&["gateway", "stop"]) {
        Ok(msg) => eprintln!("[shutdown] gateway stopped: {msg}"),
        Err(e) => eprintln!("[shutdown] gateway stop failed: {e}"),
    }
}

fn main() {
    // Catch SIGTERM/SIGINT (macOS/Linux Dock quit, Ctrl+C) to stop gateway before exit.
    #[cfg(unix)]
    {
        std::thread::spawn(|| {
            unsafe {
                libc::signal(libc::SIGTERM, handle_sigterm as usize);
                libc::signal(libc::SIGINT, handle_sigterm as usize);
            }
            loop {
                std::thread::sleep(std::time::Duration::from_millis(200));
                if SIGTERM_RECEIVED.load(Ordering::Relaxed) {
                    stop_gateway_sync();
                    std::process::exit(0);
                }
            }
        });
    }

    // Windows: catch Ctrl+C / console close to stop gateway before exit.
    #[cfg(windows)]
    {
        unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
            SIGTERM_RECEIVED.store(true, Ordering::Relaxed);
            stop_gateway_sync();
            0 // allow default handler to terminate
        }
        unsafe extern "system" { fn SetConsoleCtrlHandler(handler: unsafe extern "system" fn(u32) -> i32, add: i32) -> i32; }
        unsafe { SetConsoleCtrlHandler(handler, 1); }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Second instance launched — focus the existing window instead.
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            stream::stream_fetch,
            start_gateway,
            stop_gateway,
            gateway_status,
            get_config_path,
            clear_webview_cache_dirs,
            run_setup,
            write_config,
            read_config_file,
            get_channel_accounts,
            check_setup,
            get_gateway_port,
            get_version,
            scan_openclaw,
            detect_openclaw,
            channel_login_start,
            channel_login_qr,
            channel_login_status,
            get_cron_jobs,
            save_cron_jobs,
            get_skills,
            install_skill,
            uninstall_skill,
            search_skills,
            test_provider,
            write_workspace_file,
            read_workspace_file,
            run_rsclaw_cli,
            migrate_openclaw,
            set_auto_start,
            get_auto_start,
            set_gateway_user_stopped,
            open_path,
            save_attach_image,
            read_file_as_data_url,
            open_glow_overlay,
            close_glow_overlay,
        ])
        .setup(|app| {
            // Seed the bundled BGE embedding model into the standard rsclaw
            // models directory so the gateway start path finds it without
            // touching the network. Idempotent: skips when model.safetensors
            // already exists at the target. Runs on a blocking thread —
            // copying the ~91MB safetensors blob would otherwise stall the
            // splash window for several seconds on slow disks.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(e) = seed_bundled_bge_model(&handle) {
                    eprintln!("[setup] BGE model seeding failed (gateway will fall back to download): {e:#}");
                }
            });

            // Build system tray. Labels are localized off `gateway.language`
            // (rsclaw.json5) with system-locale + English fallbacks; IDs stay
            // ASCII so the menu-event router below is locale-independent.
            let lang = tray_lang();
            let open = MenuItemBuilder::with_id("open", tray_label(lang, "open")).build(app)?;
            let sep1 = PredefinedMenuItem::separator(app)?;
            let start = MenuItemBuilder::with_id("start_gw", tray_label(lang, "start_gw")).build(app)?;
            let stop = MenuItemBuilder::with_id("stop_gw", tray_label(lang, "stop_gw")).build(app)?;
            let status = MenuItemBuilder::with_id("status_gw", tray_label(lang, "status_gw")).build(app)?;
            let sep2 = PredefinedMenuItem::separator(app)?;
            let quit = MenuItemBuilder::with_id("quit", tray_label(lang, "quit")).build(app)?;

            let menu = MenuBuilder::new(app)
                .item(&open)
                .item(&sep1)
                .item(&start)
                .item(&stop)
                .item(&status)
                .item(&sep2)
                .item(&quit)
                .build()?;

            let tray_icon = {
                // On macOS, prefer @2x template image for retina menu bar
                let icon_name = if cfg!(target_os = "macos") {
                    "icons/icon-tray@2x.png"
                } else {
                    "icons/icon.png"
                };
                let icon_path = app.path().resource_dir()
                    .ok()
                    .map(|d| d.join(icon_name));
                let icon = icon_path
                    .and_then(|p| {
                        eprintln!("[tray] loading icon from: {}", p.display());
                        tauri::image::Image::from_path(&p).ok()
                    });
                if icon.is_none() {
                    eprintln!("[tray] WARNING: failed to load tray icon, using default");
                }
                icon
            };

            let mut tray_builder = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("RsClaw")
                .icon_as_template(true)
                .show_menu_on_left_click(false);
            if let Some(icon) = tray_icon {
                tray_builder = tray_builder.icon(icon);
            }
            let _tray = tray_builder
                .on_menu_event(|app, event| {
                    match event.id().as_ref() {
                        "open" => {
                            #[cfg(target_os = "macos")]
                            set_dock_visible(true);
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        "start_gw" => {
                            GATEWAY_USER_STOPPED.store(false, Ordering::Relaxed);
                            let _ = run_rsclaw_command(&["gateway", "start"]);
                        }
                        "stop_gw" => {
                            GATEWAY_USER_STOPPED.store(true, Ordering::Relaxed);
                            // Notify frontend to set userStopped flag before stopping
                            let _ = app.emit("tray-gateway-action", "stop");
                            let _ = run_rsclaw_command(&["gateway", "stop"]);
                        }
                        "status_gw" => {
                            #[cfg(target_os = "macos")]
                            set_dock_visible(true);
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                                let _ = window.emit("tray-gateway-action", "status");
                            }
                        }
                        "quit" => {
                            // Notify frontend to set userStopped flag
                            let _ = app.emit("tray-gateway-action", "quit");
                            APP_EXITING.store(true, Ordering::Relaxed);
                            // Prefer HTTP shutdown — works regardless of
                            // sidecar path or working directory.
                            eprintln!("[tray quit] POST /api/v1/shutdown ...");
                            match http_shutdown_gateway() {
                                Ok(()) => eprintln!("[tray quit] shutdown OK"),
                                Err(e) => {
                                    eprintln!("[tray quit] HTTP shutdown failed: {e}; falling back to CLI");
                                    let _ = run_rsclaw_command(&["gateway", "stop"]);
                                }
                            }
                            std::thread::sleep(std::time::Duration::from_millis(600));
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                                #[cfg(target_os = "macos")]
                                set_dock_visible(false);
                            } else {
                                #[cfg(target_os = "macos")]
                                set_dock_visible(true);
                                let _ = window.show();
                                let _ = window.unminimize();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            // Only open devtools in debug builds (feature enabled by `tauri dev`).
            #[cfg(feature = "devtools")]
            if let Some(window) = app.get_webview_window("main") {
                window.open_devtools();
            }

            // Gateway health watchdog: check every 10s, auto-restart if crashed.
            // Disabled when user manually stops gateway (GATEWAY_USER_STOPPED).
            std::thread::spawn(|| {
                let mut fail_count: u32 = 0;
                const MAX_FAILS: u32 = 3;
                // Wait for initial startup
                std::thread::sleep(std::time::Duration::from_secs(10));
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    if GATEWAY_USER_STOPPED.load(Ordering::Relaxed) || APP_EXITING.load(Ordering::Relaxed) {
                        fail_count = 0;
                        continue;
                    }
                    // Simple TCP connect check — if gateway is listening, it's alive.
                    let healthy = std::net::TcpStream::connect_timeout(
                        &"127.0.0.1:18888".parse().expect("valid addr"),
                        std::time::Duration::from_secs(2),
                    ).is_ok();
                    if healthy {
                        fail_count = 0;
                    } else {
                        fail_count += 1;
                        eprintln!("[watchdog] gateway health check failed ({fail_count}/{MAX_FAILS})");
                        if fail_count >= MAX_FAILS {
                            eprintln!("[watchdog] restarting gateway...");
                            let _ = start_gateway();
                            fail_count = 0;
                            // Wait for restart
                            std::thread::sleep(std::time::Duration::from_secs(10));
                        }
                    }
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            match event {
                // macOS: clicking dock icon restores hidden window (native v2 support)
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Reopen { .. } => {
                    set_dock_visible(true);
                    if let Some(window) = app_handle.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.unminimize();
                        let _ = window.set_focus();
                    }
                }
                // Window close:
                // - Gateway stopped by user → quit app
                // - Gateway running → hide to tray
                tauri::RunEvent::WindowEvent {
                    event: tauri::WindowEvent::CloseRequested { api, .. },
                    ..
                } => {
                    if GATEWAY_USER_STOPPED.load(Ordering::Relaxed) {
                        // Gateway already stopped — let the window close (app will exit)
                        stop_gateway_sync();
                    } else {
                        // Gateway running — hide to tray instead of quitting
                        api.prevent_close();
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.hide();
                        }
                        #[cfg(target_os = "macos")]
                        set_dock_visible(false);
                    }
                }
                // macOS Dock quit / Cmd+Q: stop gateway before exit.
                tauri::RunEvent::ExitRequested { .. } => {
                    if !APP_EXITING.swap(true, Ordering::SeqCst) {
                        eprintln!("[ExitRequested] stopping gateway...");
                        match http_shutdown_gateway() {
                            Ok(()) => eprintln!("[ExitRequested] shutdown OK"),
                            Err(e) => {
                                eprintln!("[ExitRequested] HTTP shutdown failed: {e}; falling back to CLI");
                                let _ = run_rsclaw_command(&["gateway", "stop"]);
                            }
                        }
                    }
                }
                _ => {}
            }
        });
}
