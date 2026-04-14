//! rsclaw — AI Automation Manager Compatible with OpenClaw
//!
//! Architecture reference: AGENTS.md
//! Entry point: detects memory tier, sets up tokio runtime, dispatches CLI.

// Lint policy (AGENTS.md §18)
#![deny(clippy::unwrap_used)]
#![deny(clippy::panic)]
#![allow(clippy::large_futures)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

use anyhow::Result;
use clap::Parser;
use cli::{AcpCommand, Cli, Command};
use cmd::{
    cmd_agent_turn, cmd_agents, cmd_approvals, cmd_backup, cmd_channels, cmd_completion,
    cmd_config, cmd_configure, cmd_cron, cmd_daemon, cmd_dashboard, cmd_devices, cmd_directory,
    cmd_dns, cmd_docs, cmd_doctor, cmd_gateway, cmd_health, cmd_hooks, cmd_logs, cmd_memory,
    cmd_message, cmd_migrate, cmd_models, cmd_onboard, cmd_plugins, cmd_qr, cmd_reset, cmd_sandbox,
    cmd_secrets, cmd_security, cmd_sessions, cmd_setup, cmd_skills, cmd_status, cmd_system,
    cmd_tools, cmd_tray, cmd_tui, cmd_uninstall, cmd_update, cmd_webhooks,
};
use rsclaw::{cli, cmd, sys};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    // Install the TLS crypto provider (aws-lc-rs) before any HTTP client is
    // created.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    // Detect available memory before spinning up the runtime.
    let tier = sys::detect_memory_tier();

    // Build an appropriate tokio runtime for the detected hardware tier.
    let rt = sys::build_runtime(tier)?;
    rt.block_on(run())
}

/// Resolve the rsclaw base directory and port offset.
///
/// Priority:
///   1. `--base-dir` explicit override (ignores --dev / --profile dir logic,
///      port still applies)
///   2. `--dev`     → ~/.rsclaw-dev,    port 18889
///   3. `--profile` → ~/.rsclaw-<name>, port 18890..N (deterministic hash)
///   4. default     → ~/.rsclaw,        port 18888
const BASE_PORT: u16 = 18888;
const DEV_PORT: u16 = 18889;

fn resolve_instance(cli: &Cli) -> (std::path::PathBuf, u16) {
    let home = dirs_next::home_dir().unwrap_or_default();

    // --base-dir replaces ~/.rsclaw as the root, then --dev/--profile append
    // suffix.
    let root = cli
        .base_dir
        .as_ref()
        .map(|p| rsclaw::config::loader::expand_tilde_path_pub(p))
        .unwrap_or_else(|| home.join(".rsclaw"));

    if cli.dev {
        let dir = if cli.base_dir.is_some() {
            root.with_file_name(format!(
                "{}-dev",
                root.file_name().unwrap_or_default().to_string_lossy()
            ))
        } else {
            home.join(".rsclaw-dev")
        };
        return (dir, DEV_PORT);
    }
    if let Some(ref name) = cli.profile {
        let dir = if cli.base_dir.is_some() {
            root.with_file_name(format!(
                "{}-{name}",
                root.file_name().unwrap_or_default().to_string_lossy()
            ))
        } else {
            home.join(format!(".rsclaw-{name}"))
        };
        let offset =
            (name.bytes().fold(0u32, |a, b| a.wrapping_add(u32::from(b))) % 254) as u16 + 1;
        return (dir, DEV_PORT + offset);
    }
    (root, BASE_PORT)
}

#[allow(clippy::large_futures)]
async fn run() -> Result<()> {
    // Handle -v and -version before clap (clap handles --version and -V)
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() == 2 && (raw_args[1] == "-v" || raw_args[1] == "-version") {
        println!("rsclaw {}", env!("RSCLAW_BUILD_VERSION"));
        return Ok(());
    }

    let cli = Cli::parse();

    // Initialise logging.
    init_tracing(&cli);

    // SAFETY: single-threaded at this point (before tokio spawns).

    // Apply --base-dir / --dev / --profile instance isolation (AGENTS.md §26).
    let (base_dir, port) = resolve_instance(&cli);
    if cli.base_dir.is_some() || cli.dev || cli.profile.is_some() {
        let label = cli
            .base_dir
            .as_deref()
            .unwrap_or_else(|| cli.profile.as_deref().unwrap_or("dev"));
        println!(
            "profile: {label}  base: {}  port: {port}",
            base_dir.display()
        );
        unsafe {
            std::env::set_var("RSCLAW_BASE_DIR", base_dir.as_os_str());
            std::env::set_var("RSCLAW_PORT", port.to_string());
        }
    }

    // Apply --config-path override.
    if let Some(ref p) = cli.config_path {
        unsafe {
            std::env::set_var("RSCLAW_CONFIG_PATH", p);
        }
    }

    // Propagate --no-color as NO_COLOR env var for style helpers.
    if cli.no_color {
        unsafe {
            std::env::set_var("NO_COLOR", "1");
        }
    }

    // Warn about --container (not yet implemented).
    if let Some(ref name) = cli.container {
        println!(
            "warning: --container '{name}' specified but container support is not yet implemented"
        );
    }

    match cli.command {
        Command::Setup(args) => cmd_setup(args).await,
        Command::Onboard(args) => cmd_onboard(args).await,
        Command::Configure(args) => cmd_configure(args).await,
        Command::Config(sub) => cmd_config(sub).await,
        Command::Doctor(args) => cmd_doctor(args).await,
        Command::Gateway(sub) => cmd_gateway(sub).await,
        Command::Start => cmd_gateway(cli::GatewayCommand::Start).await,
        Command::Stop => cmd_gateway(cli::GatewayCommand::Stop).await,
        Command::Restart => cmd_gateway(cli::GatewayCommand::Restart).await,
        Command::Channels(sub) => cmd_channels(sub).await,
        Command::Agents(sub) => cmd_agents(sub).await,
        Command::Models(sub) => cmd_models(sub).await,
        Command::Skills(sub) => cmd_skills(sub).await,
        Command::Plugins(sub) => cmd_plugins(sub).await,
        Command::Message(sub) => cmd_message(sub).await,
        Command::Memory(sub) => cmd_memory(sub).await,
        Command::Migrate(args) => cmd_migrate(args).await,
        Command::Sessions(sub) => cmd_sessions(sub).await,
        Command::Cron(sub) => cmd_cron(sub).await,
        Command::Hooks(sub) => cmd_hooks(sub).await,
        Command::System(sub) => cmd_system(sub).await,
        Command::Tools(sub) => cmd_tools(sub).await,
        Command::Secrets(sub) => cmd_secrets(sub).await,
        Command::Security(sub) => cmd_security(sub).await,
        Command::Sandbox(sub) => cmd_sandbox(sub).await,
        Command::Logs(args) => cmd_logs(args).await,
        Command::Status(args) => cmd_status(args).await,
        Command::Health(args) => cmd_health(args).await,
        Command::Tui(args) => cmd_tui(args).await,
        Command::Tray => cmd_tray(),
        Command::Backup(sub) => cmd_backup(sub).await,
        Command::Reset(args) => cmd_reset(args).await,
        Command::Update(sub) | Command::Upgrade(sub) => cmd_update(sub).await,
        Command::Pairing(sub) => cmd_pairing(sub).await,
        Command::Acp(sub) => cmd_acp(sub).await,
        Command::Agent(sub) => cmd_agent(sub).await,
        Command::Approvals(sub) => cmd_approvals(sub).await,
        Command::Devices(sub) => cmd_devices(sub).await,
        Command::Directory(sub) => cmd_directory(sub).await,
        Command::Dns(sub) => cmd_dns(sub).await,
        Command::AgentTurn(args) => cmd_agent_turn(args).await,
        Command::Completion(args) => cmd_completion(args).await,
        Command::Dashboard { no_open } => cmd_dashboard(no_open).await,
        Command::Daemon(sub) => cmd_daemon(sub).await,
        Command::Docs { query } => cmd_docs(query).await,
        Command::Qr(args) => cmd_qr(args).await,
        Command::Uninstall(args) => cmd_uninstall(args).await,
        Command::Webhooks(sub) => cmd_webhooks(sub).await,
    }
}

// ---------------------------------------------------------------------------
// Pairing command
// ---------------------------------------------------------------------------

async fn cmd_pairing(sub: cli::PairingCommand) -> Result<()> {
    match sub {
        cli::PairingCommand::Approve { code } => {
            // Try to call running gateway API.
            let config = rsclaw::config::load().ok();
            let port = std::env::var("RSCLAW_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| config.as_ref().map_or(18888, |c| c.gateway.port));
            let auth_token_val = config
                .as_ref()
                .and_then(|c| c.gateway.auth_token.clone())
                .or_else(|| std::env::var("RSCLAW_AUTH_TOKEN").ok())
                .unwrap_or_default();
            let auth_token = auth_token_val.as_str();
            let url = format!("http://127.0.0.1:{port}/api/v1/channels/pair");
            let client = reqwest::Client::new();
            let resp = client
                .post(&url)
                .header("Authorization", format!("Bearer {auth_token}"))
                .json(&serde_json::json!({ "code": code }))
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let body: serde_json::Value = r.json().await.unwrap_or_default();
                    let peer = body["peerId"].as_str().unwrap_or("unknown");
                    let channel = body["channel"].as_str().unwrap_or("unknown");
                    println!("  [ok] Approved peer {peer} on {channel}");
                }
                _ => {
                    println!("  [!] Gateway not reachable at port {port}");
                    println!("      Start the gateway first: rsclaw gateway start");
                }
            }
        }
        cli::PairingCommand::Revoke { channel, peer } => {
            Box::pin(cmd::channels::cmd_channels(cli::ChannelsCommand::Unpair {
                channel,
                peer,
            }))
            .await?;
        }
        cli::PairingCommand::List => {
            Box::pin(cmd::channels::cmd_channels(cli::ChannelsCommand::Paired {
                channel: None,
            }))
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ACP commands
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
async fn cmd_acp(sub: AcpCommand) -> Result<()> {
    use rsclaw::acp::{GatewayClient, client::AcpClient};

    match sub {
        AcpCommand::Spawn { command, cwd, args } => {
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .expect("current_dir")
                    .to_string_lossy()
                    .to_string()
            });
            let mut cmd_args = vec!["acp".to_string()];
            cmd_args.extend(args);

            eprintln!("Spawning {} with args {:?}", command, cmd_args);

            let client = AcpClient::spawn(
                &command,
                &cmd_args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            )
            .await?;

            let init_resp = client
                .initialize("rsclaw", env!("RSCLAW_BUILD_VERSION"))
                .await?;
            eprintln!(
                "Agent initialized: {} v{}",
                init_resp.agent_info.name, init_resp.agent_info.version
            );

            let session_resp = client.create_session(&cwd, None, None).await?;
            eprintln!("Session created: {}", session_resp.session_id);

            interactive_loop(&client).await?;
            client.shutdown().await?;
            Ok(())
        }

        AcpCommand::Connect {
            url,
            token,
            password: _,
            cwd,
            label,
            model,
        } => {
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .expect("current_dir")
                    .to_string_lossy()
                    .to_string()
            });

            eprintln!("Connecting to Gateway: {}", url);

            let client = GatewayClient::connect(
                &url,
                "rsclaw:client",
                env!("RSCLAW_BUILD_VERSION"),
                token.as_deref(),
                None,
            )
            .await?;
            eprintln!("Connected");

            eprintln!("Spawning agent in: {}", cwd);
            let info = client
                .spawn_agent(&cwd, model.as_deref(), label.as_deref())
                .await?;
            eprintln!(
                "Agent spawned: {} (session: {})",
                info.get("agentId").and_then(|v| v.as_str()).unwrap_or("?"),
                info.get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
            );

            interactive_gateway_loop(&client).await?;

            if let Some(agent_id) = info.get("agentId").and_then(|v| v.as_str()) {
                eprintln!("Killing agent: {}", agent_id);
                client.kill_agent(agent_id).await?;
            }
            client.close().await?;
            Ok(())
        }

        AcpCommand::Run {
            task,
            session_id,
            cwd,
            command,
        } => {
            let task = task.join(" ");
            if task.is_empty() {
                anyhow::bail!("No task provided. Usage: rsclaw acp run <task>");
            }

            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .expect("current_dir")
                    .to_string_lossy()
                    .to_string()
            });

            eprintln!(
                "[rsclaw] Running task with {}: {}",
                command,
                &task[..task.len().min(50)]
            );

            let client = AcpClient::spawn(&command, &["acp"]).await?;

            let init_resp = client
                .initialize("rsclaw", env!("RSCLAW_BUILD_VERSION"))
                .await?;
            eprintln!(
                "[rsclaw] Agent initialized: {} v{}",
                init_resp.agent_info.name, init_resp.agent_info.version
            );

            if session_id.is_some() {
                eprintln!("[rsclaw] Session resume not yet implemented, creating new session");
            }
            eprintln!("[rsclaw] Creating new session in: {}", cwd);
            let resp = client.create_session(&cwd, None, None).await?;
            eprintln!("[rsclaw] Session: {}", resp.session_id);

            eprintln!("[rsclaw] Sending prompt...");
            let resp = client.send_prompt(&task).await?;

            if let Some(result) = resp.result {
                for block in result.content {
                    match block {
                        rsclaw::acp::types::ContentBlock::Text { text } => {
                            println!("{}", text);
                        }
                        rsclaw::acp::types::ContentBlock::Image { source, .. } => {
                            println!("[Image: {}]", source.type_);
                        }
                        rsclaw::acp::types::ContentBlock::Resource { resource } => {
                            println!("[Resource: {}]", resource.uri);
                        }
                        _ => {}
                    }
                }
                if let Some(tool_calls) = result.tool_calls {
                    eprintln!("\n[Tool calls: {}]", tool_calls.len());
                }
            }
            eprintln!("\n[rsclaw] Stop reason: {:?}", resp.stop_reason);

            client.shutdown().await?;
            eprintln!("[rsclaw] Done");
            Ok(())
        }

        AcpCommand::List { url, token } => {
            eprintln!("Connecting to Gateway: {}", url);
            let client = GatewayClient::connect(
                &url,
                "rsclaw:client",
                env!("RSCLAW_BUILD_VERSION"),
                token.as_deref(),
                None,
            )
            .await?;

            eprintln!("Listing agents...");
            let agents = client.list_agents().await?;
            if agents.is_empty() {
                println!("No agents running");
            } else {
                for agent in agents {
                    println!(
                        "{} - {} ({})",
                        agent.id,
                        agent.label.unwrap_or_default(),
                        agent.status
                    );
                }
            }

            client.close().await?;
            Ok(())
        }

        AcpCommand::Kill {
            url,
            token,
            agent_id,
        } => {
            eprintln!("Connecting to Gateway: {}", url);
            let client = GatewayClient::connect(
                &url,
                "rsclaw:client",
                env!("RSCLAW_BUILD_VERSION"),
                token.as_deref(),
                None,
            )
            .await?;

            eprintln!("Killing agent: {}", agent_id);
            client.kill_agent(&agent_id).await?;
            eprintln!("Agent killed");

            client.close().await?;
            Ok(())
        }

        AcpCommand::Send { session_id, prompt } => {
            eprintln!("Sending to session {}: {}", session_id, prompt.join(" "));
            Ok(())
        }
    }
}

async fn interactive_loop(client: &rsclaw::acp::client::AcpClient) -> Result<()> {
    eprintln!("\nAgent ready. Type your prompt (Ctrl+D to exit):");

    let mut input = String::new();
    while std::io::stdin().read_line(&mut input)? > 0 {
        let prompt = input.trim();
        if prompt.is_empty() {
            continue;
        }

        match client.send_prompt(prompt).await {
            Ok(resp) => {
                if let Some(result) = resp.result {
                    for block in result.content {
                        if let rsclaw::acp::types::ContentBlock::Text { text } = block {
                            println!("{}", text);
                        }
                    }
                    if let Some(tool_calls) = result.tool_calls {
                        eprintln!("\n[Tool calls: {}]", tool_calls.len());
                    }
                }
                eprintln!("\n[Stop reason: {:?}]\n---", resp.stop_reason);
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }

        input.clear();
    }
    Ok(())
}

async fn interactive_gateway_loop(client: &rsclaw::acp::GatewayClient) -> Result<()> {
    eprintln!("\nAgent ready. Type your prompt (Ctrl+D to exit):");

    let mut input = String::new();
    while std::io::stdin().read_line(&mut input)? > 0 {
        let prompt = input.trim();
        if prompt.is_empty() {
            continue;
        }

        match client.send_prompt(prompt, None).await {
            Ok(resp) => {
                if let Some(output) = resp.get("output").and_then(|o| o.as_str()) {
                    println!("{}", output);
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&resp).unwrap_or_default()
                    );
                }
                eprintln!("\n---");
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }

        input.clear();
    }
    Ok(())
}

async fn cmd_agent(sub: cli::AgentCommand) -> Result<()> {
    match sub {
        cli::AgentCommand::Spawn {
            agent_type,
            cwd,
            args: _,
        } => {
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .expect("current_dir")
                    .to_string_lossy()
                    .to_string()
            });
            eprintln!("Spawning agent type: {} in {}", agent_type, cwd);
            Ok(())
        }
        cli::AgentCommand::List => {
            eprintln!("Listing agents...");
            Ok(())
        }
        cli::AgentCommand::Kill { id } => {
            eprintln!("Killing agent: {}", id);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Tracing initialisation
// ---------------------------------------------------------------------------

fn init_tracing(cli: &Cli) {
    // Only `gateway run` gets info-level logs by default.
    // All other CLI commands default to warn (silent) unless overridden.
    let is_gateway_run = matches!(&cli.command, Command::Gateway(cli::GatewayCommand::Run(_)));
    let default_level =
        cli.log_level
            .as_deref()
            .unwrap_or(if is_gateway_run { "info" } else { "warn" });
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    if cli.json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else if cli.no_color {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
