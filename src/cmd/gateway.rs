use anyhow::Result;

use super::style::{banner, dim, green, kv, red, yellow};
use crate::{cli::GatewayCommand, config, gateway, sys::detect_memory_tier};

const VERSION: &str = match option_env!("RSCLAW_BUILD_VERSION") { Some(v) => v, None => "dev" };

/// Spawn `rsclaw gateway run` as a detached background process, propagating
/// instance-isolation env vars set by `--dev` / `--profile`.
fn spawn_gateway_bg() -> Result<std::process::Child> {
    spawn_gateway_bg_pub()
}

/// Public version for use by configure restart.
pub fn spawn_gateway_bg_pub() -> Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    if let Ok(v) = std::env::var("RSCLAW_BASE_DIR") {
        cmd.env("RSCLAW_BASE_DIR", v);
    }
    if let Ok(v) = std::env::var("RSCLAW_PORT") {
        cmd.env("RSCLAW_PORT", v);
    }

    // Redirect stdout/stderr to log file for background mode
    let log_path = crate::config::loader::log_file();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let null_path = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|_| {
            std::fs::File::open(null_path).expect("failed to open null device")
        });
    let log_file2 = log_file
        .try_clone()
        .unwrap_or_else(|_| std::fs::File::open(null_path).expect("failed to open null device"));

    // Set default log level for background mode (user can override via RUST_LOG
    // env)
    if std::env::var("RUST_LOG").is_err() {
        cmd.env("RUST_LOG", "rsclaw=info");
    }
    cmd.arg("gateway")
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file2));

    // On Windows, detach the child process so it survives the parent exit.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const DETACHED_PROCESS: u32 = 0x00000008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }

    Ok(cmd.spawn()?)
}

pub async fn cmd_gateway(sub: GatewayCommand) -> Result<()> {
    match sub {
        GatewayCommand::Run(_args) => {
            // Check if setup is needed before loading config.
            if crate::migrate::check_needs_setup() {
                return Ok(());
            }

            let config = std::sync::Arc::new(config::load_quiet()?);
            let port = config.gateway.port;

            // Check if another instance is already running on this port.
            // Try binding to 127.0.0.1 first (always detects local conflicts),
            // then try the configured bind address if different.
            // Exit cleanly (exit 0) so systemd doesn't keep restarting.
            let port_in_use = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).is_err();
            if port_in_use {
                eprintln!("  [!] Port {port} already in use. Another gateway instance is running.");
                eprintln!("  [!] Exiting cleanly to avoid conflict.");
                std::process::exit(0);
            }
            let bind = match config.gateway.bind {
                crate::config::schema::BindMode::Auto
                | crate::config::schema::BindMode::Lan
                | crate::config::schema::BindMode::All => "0.0.0.0",
                crate::config::schema::BindMode::Loopback => "loopback",
                crate::config::schema::BindMode::Custom => "custom",
                crate::config::schema::BindMode::Tailnet => "tailnet",
            };
            let pid = std::process::id();
            banner(&format!("rsclaw gateway {VERSION}"));
            kv("Port:", &format!("{port} | Bind: {bind}"));
            kv("PID:", &format!("{pid}"));
            println!();

            let tier = detect_memory_tier();
            gateway::startup::start_gateway(config, tier).await
        }
        GatewayCommand::Start => {
            // Check if setup is needed.
            if crate::migrate::check_needs_setup() {
                return Ok(());
            }

            banner(&format!("rsclaw gateway {VERSION}"));
            // Check if already running
            if let Some(pid) = gateway_read_pid()
                && process_alive(pid)
            {
                println!("  {} Gateway already running (pid {pid})", yellow("[!]"));
                return Ok(());
            }

            // If a system service is installed, start via service manager
            // instead of spawning a bare process (avoids dual-start conflicts).
            if service_installed() {
                println!("  {} Service detected, starting via service manager...", dim("[..]"));
                if try_service_start() {
                    // Verify the gateway actually started (service may load OK
                    // but the binary may fail to run).
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if let Some(pid) = gateway_read_pid() {
                        if process_alive(pid) {
                            println!("  {} Gateway started (via service, pid {pid})", green("[ok]"));
                            kv("URL:", &detect_url());
                            println!();
                            return Ok(());
                        }
                    }
                    eprintln!("  {} Service loaded but gateway not running, falling back to direct start", yellow("[!]"));
                } else {
                    eprintln!("  {} Service start failed, falling back to direct start", yellow("[!]"));
                }
            }

            let child = spawn_gateway_bg()?;
            let pid = child.id();
            println!("  {} Gateway started", green("[ok]"));
            kv("PID:", &format!("{pid}"));
            kv("URL:", &detect_url());
            println!();
            Ok(())
        }
        GatewayCommand::Stop => {
            let pid_display = gateway_read_pid()
                .map(|p| format!(" (pid {p})"))
                .unwrap_or_default();
            match gateway_signal_stop() {
                Ok(()) => println!("  {} Gateway stopped{pid_display}", green("[ok]")),
                Err(e) => println!("  {} {e}", yellow("[!]")),
            }
            Ok(())
        }
        GatewayCommand::Restart => {
            banner(&format!("rsclaw gateway {VERSION}"));
            match gateway_signal_stop() {
                Ok(()) => {
                    println!("  {} Stopping...", dim("[..]"));
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                Err(_) => {
                    println!("  {} No running gateway found, starting fresh", dim("[..]"));
                }
            }

            // Prefer service manager for restart if installed.
            if service_installed() {
                if try_service_start() {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if let Some(pid) = gateway_read_pid() {
                        if process_alive(pid) {
                            println!("  {} Gateway restarted (via service, pid {pid})", green("[ok]"));
                            kv("URL:", &detect_url());
                            println!();
                            return Ok(());
                        }
                    }
                    eprintln!("  {} Service loaded but gateway not running, falling back to direct start", yellow("[!]"));
                } else {
                    eprintln!("  {} Service start failed, falling back to direct start", yellow("[!]"));
                }
            }

            let child = spawn_gateway_bg()?;
            let pid = child.id();
            println!("  {} Gateway restarted", green("[ok]"));
            kv("PID:", &format!("{pid}"));
            kv("URL:", &detect_url());
            println!();
            Ok(())
        }
        GatewayCommand::Status => gateway_print_status(),
        GatewayCommand::Health => {
            let config = config::load_quiet().ok();
            let port = config.map(|c| c.gateway.port).unwrap_or(18888);
            let url = format!("http://127.0.0.1:{port}/api/v1/health");
            match reqwest::Client::new().get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    println!("  [ok] Healthy -- {url}");
                }
                Ok(resp) => {
                    println!("  [!!] Unhealthy -- {} {url}", resp.status());
                }
                Err(_) => {
                    println!("  [!!] Unreachable -- {url}");
                }
            }
            Ok(())
        }
        GatewayCommand::Install => cmd_gateway_install().await,
        GatewayCommand::Uninstall => cmd_gateway_uninstall().await,
        GatewayCommand::Probe => {
            let config = std::sync::Arc::new(config::load_quiet()?);
            let port = config.gateway.port;
            let url = format!("http://127.0.0.1:{port}/api/v1/health");
            let resp = reqwest::Client::new()
                .get(&url)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("gateway unreachable at {url}: {e}"))?;
            println!("  {} -- {url}", resp.status());
            Ok(())
        }
        GatewayCommand::Discover => {
            println!("Scanning local network for rsclaw/openclaw gateways...");
            println!("(discovery uses mDNS/broadcast -- not yet implemented)");
            println!("Try: http://127.0.0.1:{}", detect_port());
            Ok(())
        }
        GatewayCommand::UsageCost => {
            let config = config::load_quiet().ok();
            let port = config.map(|c| c.gateway.port).unwrap_or(18888);
            let url = format!("http://127.0.0.1:{port}/api/v1/usage");
            match reqwest::Client::new().get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    println!("{}", serde_json::to_string_pretty(&body)?);
                }
                Ok(resp) => {
                    println!("usage endpoint returned: {}", resp.status());
                }
                Err(_) => {
                    println!("gateway not reachable at port {port}");
                }
            }
            Ok(())
        }
        GatewayCommand::Call { method, args } => {
            let config = std::sync::Arc::new(config::load_quiet()?);
            let port = config.gateway.port;
            let url = format!("http://127.0.0.1:{port}/api/v1/{method}");
            let body: serde_json::Value = if args.is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                serde_json::from_str(&args.join(" "))
                    .unwrap_or(serde_json::Value::String(args.join(" ")))
            };
            let resp = reqwest::Client::new()
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("gateway unreachable at {url}: {e}"))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            println!("{status} {text}");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// PID helpers
// ---------------------------------------------------------------------------

pub fn gateway_pid_file() -> std::path::PathBuf {
    config::loader::pid_file()
}

fn gateway_read_pid() -> Option<u32> {
    std::fs::read_to_string(gateway_pid_file())
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

fn process_alive(pid: u32) -> bool {
    crate::sys::process_alive(pid)
}

fn detect_port() -> u16 {
    config::load_quiet()
        .ok()
        .map(|c| c.gateway.port)
        .unwrap_or(18888)
}

/// Build the gateway URL from config (bind_address + port).
fn detect_url() -> String {
    let cfg = config::load_quiet().ok();
    let port = cfg.as_ref().map(|c| c.gateway.port).unwrap_or(18888);
    let bind = cfg
        .as_ref()
        .and_then(|c| c.gateway.bind_address.as_deref())
        .unwrap_or("127.0.0.1");
    // 0.0.0.0 means "all interfaces" but for display use 127.0.0.1.
    let display_host = if bind == "0.0.0.0" || bind == "::" { "127.0.0.1" } else { bind };
    format!("http://{display_host}:{port}")
}

pub fn gateway_signal_stop() -> Result<()> {
    // Try service manager first (handles auto-restart properly).
    if try_service_stop() {
        // Wait a moment for the service to fully stop.
        std::thread::sleep(std::time::Duration::from_secs(2));
        let _ = std::fs::remove_file(gateway_pid_file());
        return Ok(());
    }

    // Fallback: direct PID kill (for manual `gateway start` without service).
    let pid = gateway_read_pid()
        .ok_or_else(|| anyhow::anyhow!("gateway is not running (no PID file)"))?;
    if !process_alive(pid) {
        let _ = std::fs::remove_file(gateway_pid_file());
        anyhow::bail!("gateway process {pid} is not running");
    }
    crate::sys::process_terminate(pid)?;
    // Wait for process to exit (up to 5 seconds).
    for _ in 0..50 {
        if !process_alive(pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Clean up PID file.
    let _ = std::fs::remove_file(gateway_pid_file());
    Ok(())
}

/// Check if gateway is installed as a system service.
/// Returns true if the service unit/plist/sc entry exists (even if not running).
fn service_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs_next::home_dir() {
            let plist = home.join("Library/LaunchAgents/ai.rsclaw.gateway.plist");
            if plist.exists() { return true; }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(home) = dirs_next::home_dir() {
            let unit = home.join(".config/systemd/user/rsclaw-gateway.service");
            if unit.exists() { return true; }
        }
        // Also check system-level.
        let sys_unit = std::path::Path::new("/etc/systemd/system/rsclaw-gateway.service");
        if sys_unit.exists() { return true; }
    }

    #[cfg(target_os = "windows")]
    {
        // sc query returns non-zero if service doesn't exist.
        if let Ok(o) = std::process::Command::new("sc")
            .args(["query", "rsclaw"])
            .output()
        {
            // If output contains "SERVICE_NAME" then the service exists.
            if String::from_utf8_lossy(&o.stdout).contains("SERVICE_NAME") {
                return true;
            }
        }
    }

    false
}

/// Try to start gateway via service manager.
/// Returns true if the service was started successfully.
fn try_service_start() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs_next::home_dir() {
            let plist = home.join("Library/LaunchAgents/ai.rsclaw.gateway.plist");
            if plist.exists() {
                let status = std::process::Command::new("launchctl")
                    .args(["load", "-w"])
                    .arg(&plist)
                    .status();
                return status.map(|s| s.success()).unwrap_or(false);
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Try user service first.
        if let Some(home) = dirs_next::home_dir() {
            let unit = home.join(".config/systemd/user/rsclaw-gateway.service");
            if unit.exists() {
                let status = std::process::Command::new("systemctl")
                    .args(["--user", "start", "rsclaw-gateway"])
                    .status();
                return status.map(|s| s.success()).unwrap_or(false);
            }
        }
        // System-level.
        let sys_unit = std::path::Path::new("/etc/systemd/system/rsclaw-gateway.service");
        if sys_unit.exists() {
            let status = std::process::Command::new("systemctl")
                .args(["start", "rsclaw-gateway"])
                .status();
            return status.map(|s| s.success()).unwrap_or(false);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let status = std::process::Command::new("sc")
            .args(["start", "rsclaw"])
            .status();
        return status.map(|s| s.success()).unwrap_or(false);
    }

    #[allow(unreachable_code)]
    false
}

/// Try to stop gateway via service manager (launchctl/systemctl).
/// Returns true if a service was found and stop was attempted.
fn try_service_stop() -> bool {
    #[cfg(target_os = "macos")]
    {
        let plist = dirs_next::home_dir()
            .map(|h| h.join("Library/LaunchAgents/ai.rsclaw.gateway.plist"));
        if let Some(ref path) = plist {
            if path.exists() {
                // Use unload (without -w) to stop without disabling auto-start.
                let status = std::process::Command::new("launchctl")
                    .args(["unload"])
                    .arg(path)
                    .status();
                if let Ok(s) = status {
                    if s.success() {
                        return true;
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Check if systemd service exists and is active.
        let is_active = std::process::Command::new("systemctl")
            .args(["--user", "is-active", "rsclaw-gateway"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if is_active {
            let status = std::process::Command::new("systemctl")
                .args(["--user", "stop", "rsclaw-gateway"])
                .status();
            return status.map(|s| s.success()).unwrap_or(false);
        }
        // Try system-level service too.
        let is_active = std::process::Command::new("systemctl")
            .args(["is-active", "rsclaw-gateway"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if is_active {
            let status = std::process::Command::new("systemctl")
                .args(["stop", "rsclaw-gateway"])
                .status();
            return status.map(|s| s.success()).unwrap_or(false);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let is_active = std::process::Command::new("sc")
            .args(["query", "rsclaw"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("RUNNING"))
            .unwrap_or(false);
        if is_active {
            let status = std::process::Command::new("sc")
                .args(["stop", "rsclaw"])
                .status();
            return status.map(|s| s.success()).unwrap_or(false);
        }
    }

    false
}

pub fn gateway_print_status() -> Result<()> {
    let port = detect_port();
    let base = config::loader::base_dir();
    banner(&format!("rsclaw gateway {VERSION}"));

    kv("Base dir:", &format!("{}", base.display()));
    kv("Port:", &format!("{port}"));

    match gateway_read_pid() {
        Some(pid) if process_alive(pid) => {
            kv("Status:", &green(&format!("running (pid {pid})")));
            kv("URL:", &format!("http://127.0.0.1:{port}"));

            // Try to get version from health endpoint
            let url = format!("http://127.0.0.1:{port}/api/v1/status");
            if let Ok(resp) = reqwest::blocking::get(&url)
                && let Ok(body) = resp.json::<serde_json::Value>()
            {
                if let Some(v) = body.get("version").and_then(|v| v.as_str()) {
                    kv("Version:", v);
                }
                if let Some(a) = body.get("agents").and_then(|v| v.as_u64()) {
                    kv("Agents:", &format!("{a}"));
                }
            }
        }
        Some(pid) => {
            let _ = std::fs::remove_file(gateway_pid_file());
            kv("Status:", &red(&format!("stopped (stale pid {pid})")));
        }
        None => {
            kv("Status:", &red("stopped"));
        }
    }
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// gateway install / uninstall
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
async fn cmd_gateway_install() -> Result<()> {
    let home = dirs_next::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home dir"))?;
    let binary = std::env::current_exe()?;
    let plist_dir = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&plist_dir)?;
    let plist_path = plist_dir.join("ai.rsclaw.gateway.plist");

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>ai.rsclaw.gateway</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>gateway</string>
    <string>run</string>
  </array>
  <key>KeepAlive</key>
  <true/>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{home}/.rsclaw/var/logs/gateway.log</string>
  <key>StandardErrorPath</key>
  <string>{home}/.rsclaw/var/logs/gateway.log</string>
</dict>
</plist>
"#,
        binary = binary.display(),
        home = home.display(),
    );

    std::fs::write(&plist_path, &plist)?;
    println!("  [+] {}", plist_path.display());

    let status = std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist_path)
        .status()?;

    if status.success() {
        println!("  [ok] Service installed -- starts on login, restarts on crash");
    } else {
        eprintln!("  [!!] launchctl load failed (exit {})", status);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn cmd_gateway_uninstall() -> Result<()> {
    let home = dirs_next::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home dir"))?;
    let plist_path = home.join("Library/LaunchAgents/ai.rsclaw.gateway.plist");

    let status = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist_path)
        .status()?;

    if !status.success() {
        eprintln!("  [!] launchctl unload failed (may not have been loaded)");
    }

    if plist_path.exists() {
        std::fs::remove_file(&plist_path)?;
    }
    println!("  [ok] Service uninstalled");
    Ok(())
}

#[cfg(target_os = "linux")]
async fn cmd_gateway_install() -> Result<()> {
    let binary = std::env::current_exe()?;
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_owned());
    let home = dirs_next::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home dir"))?;

    let unit = format!(
        "[Unit]\n\
         Description=rsclaw AI gateway\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         ExecStart={binary} gateway run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         StandardOutput=append:{home}/.rsclaw/var/logs/gateway.log\n\
         StandardError=append:{home}/.rsclaw/var/logs/gateway.log\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        binary = binary.display(),
        home = home.display(),
    );

    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit_path = unit_dir.join("rsclaw-gateway.service");
    std::fs::write(&unit_path, &unit)?;
    println!("  [+] {}", unit_path.display());

    for cmd in [
        vec!["systemctl", "--user", "daemon-reload"],
        vec!["systemctl", "--user", "enable", "--now", "rsclaw-gateway"],
    ] {
        let status = std::process::Command::new(cmd[0])
            .args(&cmd[1..])
            .status()?;
        if !status.success() {
            eprintln!("  [!!] systemctl {} failed", cmd[1..].join(" "));
        }
    }
    println!("  [ok] Service installed and started");
    Ok(())
}

#[cfg(target_os = "linux")]
async fn cmd_gateway_uninstall() -> Result<()> {
    let home = dirs_next::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home dir"))?;
    let unit_path = home.join(".config/systemd/user/rsclaw-gateway.service");

    for cmd in [
        vec!["systemctl", "--user", "disable", "--now", "rsclaw-gateway"],
        vec!["systemctl", "--user", "daemon-reload"],
    ] {
        let _ = std::process::Command::new(cmd[0]).args(&cmd[1..]).status();
    }

    if unit_path.exists() {
        std::fs::remove_file(&unit_path)?;
    }
    println!("  [ok] Service uninstalled");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn cmd_gateway_install() -> Result<()> {
    let binary = std::env::current_exe()?;
    let binary_str = binary.to_string_lossy();

    // Register as a Windows service using sc.exe.
    // The service runs `rsclaw gateway run` in the background.
    // sc.exe requires "key= value" format (space after =, value as next arg).
    let bin_path = format!("\"{}\" gateway run", binary_str);
    let status = std::process::Command::new("sc")
        .args([
            "create", "rsclaw",
            "binPath=", &bin_path,
            "start=", "auto",
            "DisplayName=", "RsClaw AI Gateway",
        ])
        .status()?;
    if !status.success() {
        eprintln!("  [!] sc create failed. Try running as Administrator.");
        return Ok(());
    }
    println!("  [+] Service registered: rsclaw");

    // Start the service.
    let _ = std::process::Command::new("sc")
        .args(["start", "rsclaw"])
        .status();
    println!("  [ok] Service installed and started");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn cmd_gateway_uninstall() -> Result<()> {
    // Stop first.
    let _ = std::process::Command::new("sc")
        .args(["stop", "rsclaw"])
        .status();
    std::thread::sleep(std::time::Duration::from_secs(2));

    let status = std::process::Command::new("sc")
        .args(["delete", "rsclaw"])
        .status()?;
    if !status.success() {
        eprintln!("  [!] sc delete failed. Try running as Administrator.");
    } else {
        println!("  [ok] Service uninstalled");
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn cmd_gateway_install() -> Result<()> {
    println!("  [!] Gateway install is not supported on this platform");
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn cmd_gateway_uninstall() -> Result<()> {
    println!("  [!] Gateway uninstall is not supported on this platform");
    Ok(())
}
