// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
#[macro_use]
extern crate objc;

mod stream;

use tauri::api::process::Command;
use tauri::{
    CustomMenuItem, Manager, SystemTray, SystemTrayEvent, SystemTrayMenu, SystemTrayMenuItem,
};

// ---------------------------------------------------------------------------
// macOS: restore hidden window when user clicks dock icon
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod mac_dock {
    use std::sync::OnceLock;
    use tauri::Manager;
    use objc::declare::ClassDecl;
    use objc::runtime::{BOOL, Object, Sel, YES};

    static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

    extern "C" fn handle_reopen(_self: &Object, _cmd: Sel, _app: *mut Object, _has_visible: BOOL) -> BOOL {
        if let Some(handle) = APP_HANDLE.get() {
            if let Some(win) = handle.get_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }
        YES
    }

    /// Install a new NSApplicationDelegate that handles dock icon clicks.
    /// Must be called from `setup()` after Tauri has configured its own delegate.
    pub fn install(handle: tauri::AppHandle) {
        APP_HANDLE.get_or_init(|| handle);

        unsafe {
            // Create a new delegate class with the reopen handler
            if let Some(mut decl) = ClassDecl::new("RsClawDockDelegate", objc::class!(NSObject)) {
                decl.add_method(
                    objc::sel!(applicationShouldHandleReopen:hasVisibleWindows:),
                    handle_reopen as extern "C" fn(&Object, Sel, *mut Object, BOOL) -> BOOL,
                );
                let cls = decl.register();
                let delegate: *mut Object = objc::msg_send![cls, new];
                let app: *mut Object = objc::msg_send![objc::class!(NSApplication), sharedApplication];
                let _: () = objc::msg_send![app, setDelegate: delegate];
            }
        }
    }
}

/// Check if a process is alive (cross-platform).
fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill -0 checks if process exists without sending a signal
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(windows)]
    {
        // tasklist /FI checks if a specific PID exists
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

fn run_rsclaw_command(args: &[&str]) -> Result<String, String> {
    // Try sidecar first; if it doesn't exist, fall back to PATH.
    let result = Command::new_sidecar("rsclaw-cli")
        .ok()
        .and_then(|cmd| cmd.args(args).output().ok());

    let output = match result {
        Some(o) => o,
        None => {
            // Fallback: try "rsclaw" from PATH.
            let o = std::process::Command::new("rsclaw")
                .args(args)
                .output()
                .map_err(|e| format!("Failed to execute rsclaw: {}", e))?;
            // Convert to same format.
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
    };

    if output.status.success() {
        Ok(output.stdout.trim().to_string())
    } else {
        Err(format!(
            "rsclaw {} failed: {}{}",
            args.join(" "),
            output.stdout,
            output.stderr
        ))
    }
}

// -- Tauri commands for frontend --

/// Run rsclaw-cli with arbitrary arguments and return combined stdout+stderr.
#[tauri::command]
fn run_rsclaw_cli(args: Vec<String>) -> Result<String, String> {
    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    // Try sidecar first, fallback to PATH
    let result = Command::new_sidecar("rsclaw-cli")
        .ok()
        .and_then(|cmd| cmd.args(&str_args).output().ok());

    let (stdout, stderr, success) = match result {
        Some(o) => (o.stdout, o.stderr, o.status.success()),
        None => {
            let o = std::process::Command::new("rsclaw")
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
    let spawned = Command::new_sidecar("rsclaw-cli")
        .ok()
        .and_then(|cmd| cmd.args(&["gateway", "start"]).spawn().ok());
    if spawned.is_some() {
        return Ok("gateway starting (sidecar)".to_string());
    }
    // Fallback: PATH
    std::process::Command::new("rsclaw")
        .args(["gateway", "start"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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
    if let Some(home) = dirs::home_dir() {
        let rsclaw_dir = home.join(".rsclaw");
        Ok(rsclaw_dir.to_string_lossy().to_string())
    } else {
        Err("Could not determine home directory".to_string())
    }
}

/// Run initial setup: create directories + seed workspace.
#[tauri::command]
fn run_setup() -> Result<String, String> {
    run_rsclaw_command(&["setup", "--non-interactive"])
}

/// Write a file to an agent's workspace directory (~/.rsclaw/workspace-{agentId}/{fileName})
#[tauri::command]
fn write_workspace_file(agent_id: String, file_name: String, content: String) -> Result<String, String> {
    let home = dirs::home_dir().ok_or("no home dir")?;
    let ws_dir = home.join(".rsclaw").join(format!("workspace-{}", agent_id));
    let _ = std::fs::create_dir_all(&ws_dir);
    let file_path = ws_dir.join(&file_name);
    std::fs::write(&file_path, &content)
        .map_err(|e| format!("write failed: {e}"))?;
    Ok(file_path.to_string_lossy().to_string())
}

/// Read a file from an agent's workspace directory
#[tauri::command]
fn read_workspace_file(agent_id: String, file_name: String) -> Result<String, String> {
    let home = dirs::home_dir().ok_or("no home dir")?;
    let file_path = home.join(".rsclaw").join(format!("workspace-{}", agent_id)).join(&file_name);
    std::fs::read_to_string(&file_path).map_err(|e| format!("read failed: {e}"))
}

/// Write config file to ~/.rsclaw/rsclaw.json5
#[tauri::command]
fn write_config(content: String) -> Result<String, String> {
    let home = dirs::home_dir().ok_or("no home dir")?;
    let config_path = home.join(".rsclaw").join("rsclaw.json5");
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
    let home = dirs::home_dir().ok_or("no home dir")?;
    let config_path = home.join(".rsclaw").join("rsclaw.json5");
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
    let mut token = val.pointer("/gateway/auth/token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .or_else(|| std::env::var("RSCLAW_AUTH_TOKEN").ok())
        .unwrap_or_default();

    if token.is_empty() {
        // Generate a random token and write to config
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let pid = std::process::id();
        let generated = format!("{:016x}{:08x}", ts, pid);
        let mut cfg: serde_json::Value = json5::from_str(&raw).unwrap_or(serde_json::json!({}));
        if cfg.get("gateway").is_none() {
            cfg["gateway"] = serde_json::json!({});
        }
        if cfg["gateway"].get("auth").is_none() {
            cfg["gateway"]["auth"] = serde_json::json!({});
        }
        cfg["gateway"]["auth"]["token"] = serde_json::json!(generated);
        let _ = std::fs::write(&config_path, serde_json::to_string_pretty(&cfg).unwrap_or_default());
        token = generated;
    }

    Ok(serde_json::json!({
        "url": format!("http://{}:{}", host, port),
        "token": token,
    }))
}

/// Read channel accounts from config (channels.xxx.accounts keys).
#[tauri::command]
fn get_channel_accounts() -> Result<serde_json::Value, String> {
    let home = dirs::home_dir().ok_or("no home dir")?;
    let config_path = home.join(".rsclaw").join("rsclaw.json5");
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
    let home = dirs::home_dir().ok_or("no home dir")?;
    let config_path = home.join(".rsclaw").join("rsclaw.json5");
    if !config_path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&config_path).map_err(|e| e.to_string())
}

/// Check if rsclaw is already set up (config file exists).
#[tauri::command]
fn check_setup() -> Result<bool, String> {
    let home = dirs::home_dir().ok_or("no home dir")?;
    let config_path = home.join(".rsclaw").join("rsclaw.json5");
    Ok(config_path.exists())
}

/// Get rsclaw version from the sidecar/PATH binary.
#[tauri::command]
fn get_version() -> Result<String, String> {
    run_rsclaw_command(&["--version"])
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
    let home = dirs::home_dir().unwrap_or_default();
    let config_path = home.join(".rsclaw").join("rsclaw.json5");
    let mtime = std::fs::metadata(&config_path).ok().and_then(|m| m.modified().ok());
    *LOGIN_START_MTIME.lock().unwrap() = mtime.or(Some(std::time::SystemTime::now()));

    // Try sidecar spawn (non-blocking)
    let spawned = Command::new_sidecar("rsclaw-cli")
        .ok()
        .and_then(|cmd| cmd.args(&["channels", "login", &channel]).spawn().ok());
    if spawned.is_none() {
        // Fallback: spawn via PATH
        std::process::Command::new("rsclaw")
            .args(["channels", "login", &channel])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
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
    let home = dirs::home_dir().ok_or("no home dir")?;
    let config_path = home.join(".rsclaw").join("rsclaw.json5");

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

/// Write cron jobs to ~/.rsclaw/cron/jobs.json
#[tauri::command]
fn save_cron_jobs(content: String) -> Result<(), String> {
    let home = dirs::home_dir().ok_or("no home dir")?;
    let path = home.join(".rsclaw").join("cron").join("jobs.json");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, &content).map_err(|e| e.to_string())?;

    // Notify running gateway to reload cron jobs (non-blocking).
    let port = get_gateway_port_number();
    let url = format!("http://127.0.0.1:{port}/api/v1/cron/reload");
    std::thread::spawn(move || {
        if let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
        {
            let _ = client.post(&url).send();
        }
    });

    Ok(())
}

/// Get gateway port number (default 18888)
fn get_gateway_port_number() -> u64 {
    let home = dirs::home_dir().unwrap_or_default();
    let config_path = home.join(".rsclaw").join("rsclaw.json5");
    if !config_path.exists() {
        return 18888;
    }
    let raw = std::fs::read_to_string(&config_path).unwrap_or_default();
    let val: serde_json::Value = json5::from_str(&raw).unwrap_or(serde_json::json!({}));
    val.pointer("/gateway/port")
        .and_then(|v| v.as_u64())
        .unwrap_or(18888)
}

/// Read cron jobs from ~/.rsclaw/cron/jobs.json
#[tauri::command]
fn get_cron_jobs() -> Result<serde_json::Value, String> {
    let home = dirs::home_dir().ok_or("no home dir")?;
    let path = home.join(".rsclaw").join("cron").join("jobs.json");
    if !path.exists() {
        return Ok(serde_json::json!({ "jobs": [] }));
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let val: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({ "jobs": [] }));
    Ok(val)
}

/// List installed skills by reading ~/.rsclaw/skills/ directory
#[tauri::command]
fn get_skills() -> Result<serde_json::Value, String> {
    let home = dirs::home_dir().ok_or("no home dir")?;
    let skills_dir = home.join(".rsclaw").join("skills");
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

/// Test provider API key by calling /v1/models directly (no gateway needed).
#[tauri::command]
async fn test_provider(provider: String, api_key: String, base_url: Option<String>, api_type: Option<String>) -> Result<serde_json::Value, String> {
    // Resolve the effective API type for custom/codingplan providers
    let is_custom_like = provider == "custom" || provider == "codingplan";
    let effective_api_type = if is_custom_like {
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
            req = req.header("x-api-key", &api_key);
            req = req.header("anthropic-version", "2023-06-01");
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
    let app_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {}", e))?
        .to_string_lossy()
        .to_string();

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
    let app_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {}", e))?
        .to_string_lossy()
        .to_string();

    let auto = auto_launch::AutoLaunchBuilder::new()
        .set_app_name(app_name)
        .set_app_path(&app_path)
        .set_use_launch_agent(true)
        .build()
        .map_err(|e| format!("Failed to build auto-launch: {}", e))?;

    auto.is_enabled()
        .map_err(|e| format!("Failed to check auto-start status: {}", e))
}

// -- System tray --

fn build_system_tray() -> SystemTray {
    let open = CustomMenuItem::new("open".to_string(), "Open RsClaw");
    let start = CustomMenuItem::new("start_gw".to_string(), "Start Gateway");
    let stop = CustomMenuItem::new("stop_gw".to_string(), "Stop Gateway");
    let status = CustomMenuItem::new("status_gw".to_string(), "Gateway Status");
    let quit = CustomMenuItem::new("quit".to_string(), "Quit");

    let tray_menu = SystemTrayMenu::new()
        .add_item(open)
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(start)
        .add_item(stop)
        .add_item(status)
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(quit);

    SystemTray::new().with_menu(tray_menu)
}

fn handle_tray_event(app: &tauri::AppHandle, event: SystemTrayEvent) {
    match event {
        SystemTrayEvent::LeftClick { .. } => {
            // Toggle main window visibility on left click
            if let Some(window) = app.get_window("main") {
                if window.is_visible().unwrap_or(false) {
                    let _ = window.hide();
                } else {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        }
        SystemTrayEvent::MenuItemClick { id, .. } => match id.as_str() {
            "open" => {
                if let Some(window) = app.get_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "start_gw" => {
                let _ = run_rsclaw_command(&["gateway", "start"]);
            }
            "stop_gw" => {
                let _ = run_rsclaw_command(&["gateway", "stop"]);
            }
            "status_gw" => {
                if let Some(window) = app.get_window("main") {
                    let status = run_rsclaw_command(&["gateway", "status"])
                        .unwrap_or_else(|e| format!("Error: {}", e));
                    let _ = tauri::api::dialog::message(Some(&window), "Gateway Status", status);
                }
            }
            "quit" => {
                // Stop gateway (rsclaw-cli) before quitting
                let _ = stop_gateway();
                std::process::exit(0);
            }
            _ => {}
        },
        _ => {}
    }
}

fn main() {
    tauri::Builder::default()
        .system_tray(build_system_tray())
        .on_system_tray_event(handle_tray_event)
        .invoke_handler(tauri::generate_handler![
            stream::stream_fetch,
            start_gateway,
            stop_gateway,
            gateway_status,
            get_config_path,
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
        ])
        .setup(|_app| {
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                let _ = stop_gateway();
            }
        });
}
