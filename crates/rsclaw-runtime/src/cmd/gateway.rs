use anyhow::Result;
use rsclaw_cli::GatewayCommand;
use rsclaw_config as config;
use rsclaw_platform::detect_memory_tier;

use super::style::{banner, dim, green, kv, red, yellow};
use crate::gateway;

const VERSION: &str = match option_env!("RSCLAW_BUILD_VERSION") {
    Some(v) => v,
    None => "dev",
};

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
    let log_path = rsclaw_config::loader::log_file();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let null_path = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|_| std::fs::File::open(null_path).expect("failed to open null device"));
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    Ok(cmd.spawn()?)
}

pub async fn cmd_gateway(sub: GatewayCommand) -> Result<()> {
    match sub {
        GatewayCommand::Run(_args) => {
            // Check if setup is needed before loading config.
            if rsclaw_migrate::check_needs_setup() {
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
                rsclaw_config::schema::BindMode::Auto
                | rsclaw_config::schema::BindMode::Lan
                | rsclaw_config::schema::BindMode::All => "0.0.0.0",
                rsclaw_config::schema::BindMode::Loopback => "loopback",
                rsclaw_config::schema::BindMode::Custom => "custom",
                rsclaw_config::schema::BindMode::Tailnet => "tailnet",
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
            if rsclaw_migrate::check_needs_setup() {
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

            // The installed service owns only the default instance. Isolated
            // --dev/--profile/config-path instances must start their own process.
            if service_manager_allowed() && service_installed() {
                println!(
                    "  {} Service detected, starting via service manager...",
                    dim("[..]")
                );
                if try_service_start() {
                    // Verify the gateway actually started (service may load OK
                    // but the binary may fail to run).
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if let Some(pid) = gateway_read_pid() {
                        if process_alive(pid) {
                            println!(
                                "  {} Gateway started (via service, pid {pid})",
                                green("[ok]")
                            );
                            kv("URL:", &detect_url());
                            println!();
                            return Ok(());
                        }
                    }
                    eprintln!(
                        "  {} Service loaded but gateway not running, falling back to direct start",
                        yellow("[!]")
                    );
                } else {
                    eprintln!(
                        "  {} Service start failed, falling back to direct start",
                        yellow("[!]")
                    );
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
            let health = health_url();
            let restart = restart_url();
            let strategy = RestartStrategy::choose(gateway_health_reachable(&health).await);

            match strategy {
                RestartStrategy::HttpGraceful => {
                    println!("  {} Requesting graceful restart...", dim("[..]"));
                    request_graceful_restart(&restart).await?;
                    println!("  {} Waiting for gateway health...", dim("[..]"));
                    wait_for_gateway_health(&health).await?;
                    println!("  {} Gateway restarted", green("[ok]"));
                    kv("URL:", &detect_url());
                    println!();
                    Ok(())
                }
                RestartStrategy::DirectStopStart => {
                    match gateway_signal_stop() {
                        Ok(()) => println!("  {} Stopped old gateway", dim("[..]")),
                        Err(error) => match restart_fallback_after_stop_error(error) {
                            RestartFallbackDecision::StartFresh => println!(
                                "  {} No running gateway found, starting fresh",
                                dim("[..]")
                            ),
                            RestartFallbackDecision::Abort { reason } => anyhow::bail!(reason),
                        },
                    }

                    // The installed service owns only the default instance.
                    if service_manager_allowed() && service_installed() {
                        if try_service_start() {
                            wait_for_gateway_health(&health).await?;
                            println!("  {} Gateway restarted via service", green("[ok]"));
                            kv("URL:", &detect_url());
                            println!();
                            return Ok(());
                        }
                        eprintln!(
                            "  {} Service start failed, falling back to direct start",
                            yellow("[!]")
                        );
                    }

                    let child = spawn_gateway_bg()?;
                    let pid = child.id();
                    wait_for_gateway_health(&health).await?;
                    println!("  {} Gateway restarted", green("[ok]"));
                    kv("PID:", &format!("{pid}"));
                    kv("URL:", &detect_url());
                    println!();
                    Ok(())
                }
            }
        }
        GatewayCommand::Reload { scope } => {
            let config = config::load_quiet().ok();
            let port = config.as_ref().map(|c| c.gateway.port).unwrap_or(18888);
            let auth_token = config
                .and_then(|c| c.gateway.auth_token)
                .unwrap_or_default();
            let scope_param = scope
                .map(|s| s.join(","))
                .unwrap_or_else(|| "all".to_owned());
            let url = format!(
                "http://127.0.0.1:{port}/api/v1/reload?scope={scope_param}"
            );
            let client = reqwest::Client::new();
            let mut req = client.post(&url);
            if !auth_token.is_empty() {
                req = req.bearer_auth(&auth_token);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value =
                        resp.json().await.unwrap_or_default();
                    println!("  {} Reload complete", green("[ok]"));
                    if let Some(details) = body.get("details") {
                        println!("  {}", serde_json::to_string_pretty(details).unwrap_or_default());
                    }
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    anyhow::bail!("reload failed ({status}): {body}");
                }
                Err(e) => {
                    anyhow::bail!(
                        "gateway unreachable at {url} — is it running?\n  {e}"
                    );
                }
            }
            Ok(())
        }
        GatewayCommand::Status => gateway_print_status().await,
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
            let port = config.as_ref().map(|c| c.gateway.port).unwrap_or(18888);
            let auth_token = config
                .and_then(|c| c.gateway.auth_token)
                .unwrap_or_default();
            let url = format!("http://127.0.0.1:{port}/api/v1/usage");
            let mut req = reqwest::Client::new().get(&url);
            if !auth_token.is_empty() {
                req = req.bearer_auth(&auth_token);
            }
            match req.send().await {
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
            let auth_token = config.gateway.auth_token.clone().unwrap_or_default();
            let url = format!("http://127.0.0.1:{port}/api/v1/{method}");
            let body: serde_json::Value = if args.is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                serde_json::from_str(&args.join(" "))
                    .unwrap_or(serde_json::Value::String(args.join(" ")))
            };
            let mut req = reqwest::Client::new().post(&url).json(&body);
            if !auth_token.is_empty() {
                req = req.bearer_auth(&auth_token);
            }
            let resp = req
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
    rsclaw_platform::process_alive(pid)
}

/// Scan for the process listening on this instance's configured gateway port.
///
/// This is only used when the current instance's PID file is absent. Do not
/// fall back to process-name matching: that could select another `--dev` or
/// `--profile` instance.
fn find_gateway_pid() -> Option<u32> {
    let port = detect_port();
    let my_pid = std::process::id();

    // Try finding by port first (most reliable).
    #[cfg(unix)]
    {
        if let Some(pid) = find_pid_with_lsof(port, my_pid) {
            return Some(pid);
        }
        #[cfg(target_os = "linux")]
        if let Some(pid) = find_pid_with_linux_socket_tools(port, my_pid) {
            return Some(pid);
        }
    }
    #[cfg(windows)]
    {
        // netstat to find PID listening on gateway port.
        #[allow(unused_mut)]
        let mut ns = std::process::Command::new("netstat");
        ns.args(["-ano"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            ns.creation_flags(0x08000000);
        }
        let output = ns.output().ok();
        if let Some(output) = output {
            let text = String::from_utf8_lossy(&output.stdout);
            let port_str = format!(":{port}");
            for line in text.lines() {
                if line.contains(&port_str) && line.contains("LISTENING") {
                    if let Some(pid_str) = line.split_whitespace().last() {
                        if let Ok(pid) = pid_str.parse::<u32>() {
                            if pid != my_pid && process_alive(pid) {
                                return Some(pid);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(unix)]
fn find_pid_with_lsof(port: u16, my_pid: u32) -> Option<u32> {
    for lsof in ["lsof", "/usr/sbin/lsof"] {
        let Some(output) = std::process::Command::new(lsof)
            .args(["-ti", &format!(":{port}"), "-sTCP:LISTEN"])
            .output()
            .ok()
        else {
            continue;
        };
        if let Some(pid) = first_live_pid(&output.stdout, my_pid) {
            return Some(pid);
        }
    }
    None
}

#[cfg(unix)]
fn first_live_pid(output: &[u8], my_pid: u32) -> Option<u32> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .find(|&pid| pid != my_pid && process_alive(pid))
}

#[cfg(target_os = "linux")]
fn find_pid_with_linux_socket_tools(port: u16, my_pid: u32) -> Option<u32> {
    for (command, args) in [
        ("ss", &["-ltnp"][..]),
        ("netstat", &["-ltnp"][..]),
    ] {
        let Some(output) = std::process::Command::new(command).args(args).output().ok() else {
            continue;
        };
        if let Some(pid) = listener_pid_from_socket_output(&output.stdout, port, my_pid) {
            return Some(pid);
        }
    }
    None
}

#[cfg(unix)]
fn listener_pid_from_socket_output(output: &[u8], port: u16, my_pid: u32) -> Option<u32> {
    String::from_utf8_lossy(output).lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        let local_endpoint = fields.get(3)?;
        if !socket_endpoint_has_port(local_endpoint, port) {
            return None;
        }
        let pid = line
            .split("pid=")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .or_else(|| fields.last().and_then(|field| field.split('/').next()))?
            .parse::<u32>()
            .ok()?;
        (pid != my_pid && process_alive(pid)).then_some(pid)
    })
}

#[cfg(unix)]
fn socket_endpoint_has_port(endpoint: &str, port: u16) -> bool {
    endpoint
        .rsplit_once(':')
        .and_then(|(_, value)| value.parse::<u16>().ok())
        == Some(port)
}

fn detect_port() -> u16 {
    let instance_port = std::env::var("RSCLAW_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok());
    let configured_port = config::load_quiet().ok().map(|c| c.gateway.port);
    detect_port_from_sources(instance_port, configured_port)
}

fn detect_port_from_sources(
    instance_port: Option<u16>,
    configured_port: Option<u16>,
) -> u16 {
    instance_port.or(configured_port).unwrap_or(18888)
}

fn gateway_target_pid() -> Option<u32> {
    select_gateway_pid(gateway_read_pid(), find_gateway_pid())
}

fn select_gateway_pid(pid_file_pid: Option<u32>, port_listener_pid: Option<u32>) -> Option<u32> {
    pid_file_pid.or(port_listener_pid)
}

/// Whether the global service manager may control the current instance.
///
/// `run` sets these overrides for `--dev`, `--profile`, `--base-dir`, and
/// `--config-path`; none of those isolated instances may start or stop the
/// globally installed default service.
fn service_manager_allowed() -> bool {
    service_manager_allowed_with_overrides(
        std::env::var_os("RSCLAW_BASE_DIR").is_some(),
        std::env::var_os("RSCLAW_CONFIG_PATH").is_some(),
    )
}

fn service_manager_allowed_with_overrides(
    base_dir_overridden: bool,
    config_path_overridden: bool,
) -> bool {
    !base_dir_overridden && !config_path_overridden
}

#[cfg(test)]
mod tests {
    use super::{
        detect_port_from_sources, select_gateway_pid, service_manager_allowed_with_overrides,
    };
    #[cfg(unix)]
    use super::socket_endpoint_has_port;

    #[test]
    fn gateway_pid_file_takes_priority_over_port_listener() {
        assert_eq!(select_gateway_pid(Some(101), Some(202)), Some(101));
    }

    #[test]
    fn gateway_port_listener_is_used_when_pid_file_is_absent() {
        assert_eq!(select_gateway_pid(None, Some(202)), Some(202));
        assert_eq!(select_gateway_pid(None, None), None);
    }

    #[test]
    fn default_instance_port_is_18888_without_config() {
        assert_eq!(detect_port_from_sources(None, None), 18888);
        assert_eq!(detect_port_from_sources(None, Some(19000)), 19000);
    }

    #[test]
    fn isolated_instance_port_overrides_missing_or_default_config() {
        assert_eq!(detect_port_from_sources(Some(18889), None), 18889);
        assert_eq!(detect_port_from_sources(Some(19100), Some(18888)), 19100);
    }

    #[test]
    fn isolated_instances_cannot_manage_global_service() {
        assert!(service_manager_allowed_with_overrides(false, false));
        assert!(!service_manager_allowed_with_overrides(true, false));
        assert!(!service_manager_allowed_with_overrides(false, true));
    }

    #[cfg(unix)]
    #[test]
    fn socket_port_matching_is_exact() {
        assert!(socket_endpoint_has_port("127.0.0.1:19142", 19142));
        assert!(socket_endpoint_has_port("[::1]:19142", 19142));
        assert!(!socket_endpoint_has_port("127.0.0.1:119142", 19142));
    }
}

/// Maximum time to wait for a gateway process to exit during CLI stop.
pub const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const STOP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// Maximum time to wait for the restarted gateway health endpoint.
///
/// Must comfortably exceed the gateway's OWN graceful-drain budget so a
/// restart that waits on an in-flight request isn't falsely reported as
/// failed. On `/restart` the old process: stops serving health, drains
/// in-flight HTTP, then waits up to 60s for non-HTTP inflight to clear
/// (`gateway::startup` drain loop), THEN re-execs the child, which must
/// init (KB rebuild, model load, channel WS connects) before it binds and
/// health goes green. 75s was shorter than 60s drain + child init, so a
/// busy gateway (an active agent turn or cap session holding inflight) timed
/// out on the first restart and only the second — with nothing in flight —
/// succeeded. 150s leaves ~90s of init headroom on top of the 60s drain cap.
pub const START_HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(150);
const START_HEALTH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Result of waiting for a gateway process to exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopWaitOutcome {
    pid: u32,
    stopped: bool,
}

impl StopWaitOutcome {
    /// Builds a wait outcome from sampled process liveness values.
    pub fn from_alive_samples<I>(pid: u32, samples: I) -> Self
    where
        I: IntoIterator<Item = bool>,
    {
        let stopped = samples.into_iter().any(|alive| !alive);
        Self { pid, stopped }
    }

    /// Returns true when the process disappeared before timeout.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Returns the timeout error message for failed stop waits.
    pub fn error_message(&self) -> Option<String> {
        if self.stopped {
            None
        } else {
            Some(format!(
                "gateway process {} did not stop before timeout",
                self.pid
            ))
        }
    }
}

/// Result of waiting for the gateway health endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthWaitOutcome {
    Healthy,
    Timeout { url: String },
}

/// Converts health probe samples into a health wait outcome.
pub fn health_wait_result<I>(probe_results: I, url: &str) -> HealthWaitOutcome
where
    I: IntoIterator<Item = bool>,
{
    if probe_results.into_iter().any(|ok| ok) {
        HealthWaitOutcome::Healthy
    } else {
        HealthWaitOutcome::Timeout {
            url: url.to_owned(),
        }
    }
}

/// Restart path selected by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    HttpGraceful,
    DirectStopStart,
}

impl RestartStrategy {
    /// Chooses graceful HTTP restart when the gateway is reachable.
    pub fn choose(gateway_reachable: bool) -> Self {
        if gateway_reachable {
            Self::HttpGraceful
        } else {
            Self::DirectStopStart
        }
    }
}

/// Decision for direct restart fallback after attempting to stop the old
/// gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartFallbackDecision {
    StartFresh,
    Abort { reason: String },
}

/// Substring that `gateway_signal_stop` uses when no gateway process is found.
/// Matched in `restart_fallback_after_stop_result` to distinguish
/// "gateway never started" from "gateway failed to stop".
const GATEWAY_NOT_RUNNING_MSG: &str = "gateway is not running";

/// Decides whether direct restart fallback may start a new gateway.
pub fn restart_fallback_after_stop_result(
    stop_result: std::result::Result<(), &str>,
) -> RestartFallbackDecision {
    match stop_result {
        Ok(()) => RestartFallbackDecision::StartFresh,
        Err(message)
            if message.starts_with(GATEWAY_NOT_RUNNING_MSG)
                || message.contains("is not running") =>
        {
            RestartFallbackDecision::StartFresh
        }
        Err(message) => RestartFallbackDecision::Abort {
            reason: message.to_owned(),
        },
    }
}

fn restart_fallback_after_stop_error(error: anyhow::Error) -> RestartFallbackDecision {
    let message = error.to_string();
    restart_fallback_after_stop_result(Err(&message))
}

fn health_url() -> String {
    let port = detect_port();
    format!("http://127.0.0.1:{port}/api/v1/health")
}

fn restart_url() -> String {
    let port = detect_port();
    format!("http://127.0.0.1:{port}/api/v1/restart")
}

fn gateway_auth_token() -> String {
    config::load_quiet()
        .ok()
        .and_then(|c| c.gateway.auth_token)
        .unwrap_or_default()
}

async fn gateway_health_reachable(url: &str) -> bool {
    reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map(|resp| resp.status().is_success())
        .unwrap_or(false)
}

async fn request_graceful_restart(url: &str) -> Result<()> {
    let token = gateway_auth_token();
    let mut req = reqwest::Client::new().post(url);
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("failed to request graceful restart at {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("graceful restart request failed: {} {url}", resp.status());
    }
    Ok(())
}

/// Returns true when the PID file should be removed after a stop attempt.
pub fn should_remove_pid_after_stop(outcome: &StopWaitOutcome) -> bool {
    outcome.is_stopped()
}

async fn wait_for_gateway_health(url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + START_HEALTH_TIMEOUT;
    let mut samples = Vec::new();

    loop {
        let ok = match client.get(url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        };
        samples.push(ok);
        match health_wait_result(samples.iter().copied(), url) {
            HealthWaitOutcome::Healthy => return Ok(()),
            HealthWaitOutcome::Timeout { .. } => {}
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "gateway did not become healthy at {url}; check {}",
                rsclaw_config::loader::log_file().display()
            );
        }
        tokio::time::sleep(START_HEALTH_POLL_INTERVAL).await;
    }
}

fn wait_for_process_exit(pid: u32) -> StopWaitOutcome {
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    loop {
        if !process_alive(pid) {
            return StopWaitOutcome { pid, stopped: true };
        }
        if std::time::Instant::now() >= deadline {
            return StopWaitOutcome {
                pid,
                stopped: false,
            };
        }
        std::thread::sleep(STOP_POLL_INTERVAL);
    }
}

/// Build the gateway URL from config (bind_address + port).
fn detect_url() -> String {
    let cfg = config::load_quiet().ok();
    let port = detect_port();
    let bind = cfg
        .as_ref()
        .and_then(|c| c.gateway.bind_address.as_deref())
        .unwrap_or("127.0.0.1");
    // 0.0.0.0 means "all interfaces" but for display use 127.0.0.1.
    let display_host = if bind == "0.0.0.0" || bind == "::" {
        "127.0.0.1"
    } else {
        bind
    };
    format!("http://{display_host}:{port}")
}

pub fn gateway_signal_stop() -> Result<()> {
    // Try service manager first (handles auto-restart properly).
    if service_manager_allowed() && try_service_stop() {
        if let Some(pid) = gateway_target_pid() {
            let outcome = wait_for_process_exit(pid);
            if should_remove_pid_after_stop(&outcome) {
                let _ = std::fs::remove_file(gateway_pid_file());
                return Ok(());
            }
            anyhow::bail!(
                outcome
                    .error_message()
                    .expect("timeout has an error message")
            );
        }
        let _ = std::fs::remove_file(gateway_pid_file());
        return Ok(());
    }

    // Fallback: direct PID kill (for manual `gateway start` without service).
    // Try this instance's PID file first, then its configured port listener.
    let pid = gateway_target_pid().ok_or_else(|| {
        anyhow::anyhow!("gateway is not running (no PID file and no matching process)")
    })?;
    if !process_alive(pid) {
        let _ = std::fs::remove_file(gateway_pid_file());
        anyhow::bail!("gateway process {pid} is not running");
    }
    rsclaw_platform::process_terminate(pid)?;
    let outcome = wait_for_process_exit(pid);
    if should_remove_pid_after_stop(&outcome) {
        let _ = std::fs::remove_file(gateway_pid_file());
        Ok(())
    } else {
        anyhow::bail!(
            outcome
                .error_message()
                .expect("timeout has an error message")
        );
    }
}

/// Check if gateway is installed as a system service.
/// Returns true if the service unit/plist/sc entry exists (even if not
/// running).
fn service_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs_next::home_dir() {
            let plist = home.join("Library/LaunchAgents/ai.rsclaw.gateway.plist");
            if plist.exists() {
                return true;
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(home) = dirs_next::home_dir() {
            let unit = home.join(".config/systemd/user/rsclaw-gateway.service");
            if unit.exists() {
                return true;
            }
        }
        // Also check system-level.
        let sys_unit = std::path::Path::new("/etc/systemd/system/rsclaw-gateway.service");
        if sys_unit.exists() {
            return true;
        }
    }

    #[cfg(target_os = "windows")]
    {
        // sc query returns non-zero if service doesn't exist.
        #[allow(unused_mut)]
        let mut sc_cmd = std::process::Command::new("sc");
        sc_cmd.args(["query", "rsclaw"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            sc_cmd.creation_flags(0x08000000);
        }
        if let Ok(o) = sc_cmd.output() {
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
        use std::os::windows::process::CommandExt;
        let status = std::process::Command::new("sc")
            .args(["start", "rsclaw"])
            .creation_flags(0x08000000)
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
        let plist =
            dirs_next::home_dir().map(|h| h.join("Library/LaunchAgents/ai.rsclaw.gateway.plist"));
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
        use std::os::windows::process::CommandExt;
        let is_active = std::process::Command::new("sc")
            .args(["query", "rsclaw"])
            .creation_flags(0x08000000)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("RUNNING"))
            .unwrap_or(false);
        if is_active {
            let status = std::process::Command::new("sc")
                .args(["stop", "rsclaw"])
                .creation_flags(0x08000000)
                .status();
            return status.map(|s| s.success()).unwrap_or(false);
        }
    }

    false
}

pub async fn gateway_print_status() -> Result<()> {
    let port = detect_port();
    let base = config::loader::base_dir();
    banner(&format!("rsclaw gateway {VERSION}"));

    kv("Base dir:", &format!("{}", base.display()));
    kv("Port:", &format!("{port}"));

    match gateway_read_pid() {
        Some(pid) if process_alive(pid) => {
            kv("Status:", &green(&format!("running (pid {pid})")));
            kv("URL:", &format!("http://127.0.0.1:{port}"));

            // Try to get version from the status endpoint. Attach the
            // gateway auth token when configured — without it the call
            // returns 401 and the gateway log fills with WARN
            // "auth rejected: missing or invalid Bearer token path=/api/v1/status"
            // every time someone runs `rsclaw status`. Uses async reqwest
            // because gateway_print_status runs inside the tokio runtime
            // (cmd_gateway is async) and reqwest::blocking would panic on
            // its internal runtime drop.
            let url = format!("http://127.0.0.1:{port}/api/v1/status");
            let auth_token = config::load()
                .ok()
                .and_then(|c| c.gateway.auth_token.clone())
                .unwrap_or_default();
            let mut req = reqwest::Client::new().get(&url);
            if !auth_token.is_empty() {
                req = req.bearer_auth(&auth_token);
            }
            if let Ok(resp) = req.send().await
                && let Ok(body) = resp.json::<serde_json::Value>().await
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

    let log_path = rsclaw_config::loader::log_file();
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
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        binary = binary.display(),
        log = log_path.display(),
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

    let log_path = rsclaw_config::loader::log_file();
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
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        binary = binary.display(),
        log = log_path.display(),
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
    use std::os::windows::process::CommandExt;
    let status = std::process::Command::new("sc")
        .args([
            "create",
            "rsclaw",
            "binPath=",
            &bin_path,
            "start=",
            "auto",
            "DisplayName=",
            "RsClaw AI Gateway",
        ])
        .creation_flags(0x08000000)
        .status()?;
    if !status.success() {
        eprintln!("  [!] sc create failed. Try running as Administrator.");
        return Ok(());
    }
    println!("  [+] Service registered: rsclaw");

    // Start the service.
    let _ = std::process::Command::new("sc")
        .args(["start", "rsclaw"])
        .creation_flags(0x08000000)
        .status();
    println!("  [ok] Service installed and started");
    Ok(())
}

#[cfg(target_os = "windows")]
async fn cmd_gateway_uninstall() -> Result<()> {
    use std::os::windows::process::CommandExt;
    // Stop first.
    let _ = std::process::Command::new("sc")
        .args(["stop", "rsclaw"])
        .creation_flags(0x08000000)
        .status();
    std::thread::sleep(std::time::Duration::from_secs(2));

    let status = std::process::Command::new("sc")
        .args(["delete", "rsclaw"])
        .creation_flags(0x08000000)
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
