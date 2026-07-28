//! rsclaw-runtime — composition root + RPC handlers.
//!
//! Holds the AppState/RPC-coupled "root knot" modules (a2a, cmd, cron runner,
//! gateway, hooks, server, ws) plus the process entry point [`run`]. Everything
//! else lives in the already-extracted `rsclaw-*` crates; this crate wires them
//! together. The binary (`src/main.rs`) is a thin shim that installs the TLS
//! provider, detects the memory tier, builds the tokio runtime and calls
//! [`run`].
#![recursion_limit = "256"]
// Lint policy (AGENTS.md §18)
#![deny(clippy::unwrap_used)]
#![deny(clippy::panic)]
#![allow(clippy::large_futures)]
#![allow(unused)]
// Pre-existing style lints — fix incrementally, not with a blanket allow(clippy::all).
#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::manual_strip,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::single_match,
    clippy::manual_contains,
    clippy::if_same_then_else,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::useless_format,
    clippy::unnecessary_to_owned,
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::print_literal,
    clippy::manual_pattern_char_comparison,
    clippy::doc_lazy_continuation,
    clippy::regex_creation_in_loops,
    clippy::while_let_loop,
    clippy::unnecessary_map_or,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_cast,
    clippy::ptr_arg,
    clippy::nonminimal_bool,
    clippy::new_without_default,
    clippy::manual_repeat_n,
    clippy::manual_is_multiple_of,
    clippy::derivable_impls,
    clippy::while_let_on_iterator,
    clippy::unnecessary_unwrap,
    clippy::unnecessary_sort_by,
    clippy::len_zero,
    clippy::map_clone,
    clippy::match_like_matches_macro,
    clippy::needless_return,
    clippy::redundant_field_names,
    clippy::redundant_pattern_matching,
    clippy::single_char_pattern,
    clippy::clone_on_copy,
    clippy::manual_map,
    clippy::unnecessary_filter_map,
    clippy::trim_split_whitespace,
    clippy::suspicious_to_owned,
    clippy::single_element_loop,
    clippy::result_large_err,
    clippy::redundant_locals,
    clippy::question_mark,
    clippy::needless_range_loop,
    clippy::match_single_binding,
    clippy::map_flatten,
    clippy::manual_unwrap_or_default,
    clippy::manual_split_once,
    clippy::manual_range_contains,
    clippy::manual_flatten,
    clippy::manual_div_ceil,
    clippy::manual_clamp,
    clippy::implicit_saturating_sub,
    clippy::field_reassign_with_default,
    clippy::uninlined_format_args,
    clippy::unused_async,
    clippy::match_wildcard_for_single_variants,
    clippy::map_unwrap_or,
    clippy::doc_markdown
)]

// ---------------------------------------------------------------------------
// The 7 root-knot modules (composition root + RPC handlers).
// ---------------------------------------------------------------------------
pub mod a2a;
pub mod cmd;
pub mod cron;
pub mod gateway;
pub mod hooks;
pub mod server;
pub mod ws;

// ---------------------------------------------------------------------------
// Re-export the extracted base crates under their historical crate-root names
// so bare `config::load()`, `agent::AgentRegistry`, `channel::...`, `sys::...`
// etc. inside the moved modules keep resolving exactly as they did when these
// modules lived in the root crate (where the old `src/lib.rs` provided the same
// aliases). Names are distinct from the 7 bundled modules above, so no clash.
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Process entry point — dispatch logic relocated from the old src/main.rs.
// ---------------------------------------------------------------------------
use anyhow::Result;
use clap::Parser;
use cmd::{
    cmd_agent_turn, cmd_agents, cmd_anycli, cmd_approvals, cmd_backup, cmd_browser, cmd_channels,
    cmd_completion, cmd_config, cmd_configure, cmd_cron, cmd_daemon, cmd_dashboard, cmd_debug,
    cmd_devices, cmd_directory, cmd_dns, cmd_docs, cmd_doctor, cmd_env, cmd_gateway, cmd_health,
    cmd_hooks, cmd_image, cmd_kb, cmd_logs, cmd_memory, cmd_message, cmd_migrate, cmd_models,
    cmd_onboard, cmd_plugins, cmd_qr, cmd_reset, cmd_sandbox, cmd_secrets, cmd_security,
    cmd_sessions, cmd_setup, cmd_skills, cmd_status, cmd_system, cmd_tools, cmd_tray, cmd_tui,
    cmd_uninstall, cmd_update, cmd_watch, cmd_webhooks,
};
pub use rsclaw_agent as agent;
pub use rsclaw_artifact as artifact;
pub use rsclaw_browser as browser;
pub use rsclaw_cap as cap;
pub use rsclaw_channel as channel;
pub use rsclaw_cli as cli;
use rsclaw_cli::{Cli, Command};
pub use rsclaw_computer as computer;
pub use rsclaw_config as config;
pub use rsclaw_desktop as desktop;
pub use rsclaw_embed as embed;
pub use rsclaw_events as events;
pub use rsclaw_heartbeat as heartbeat;
pub use rsclaw_i18n as i18n;
pub use rsclaw_kb as kb;
pub use rsclaw_mcp as mcp;
pub use rsclaw_migrate as migrate;
pub use rsclaw_platform as sys;
pub use rsclaw_platform::MemoryTier;
pub use rsclaw_plugin as plugin;
pub use rsclaw_provider as provider;
pub use rsclaw_skill as skill;
pub use rsclaw_store as store;
pub use rsclaw_util as util;
use tracing_subscriber::EnvFilter;

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

/// Best-effort read of `gateway.port` from `<base_dir>/rsclaw.json5`.
/// Returns None when the file doesn't exist, doesn't parse, or doesn't set
/// the field — caller falls back to the auto-computed offset. Intentionally
/// uses raw JSON5 here (not the schema deserializer) so a malformed
/// unrelated config field never blocks port resolution.
fn read_config_port(base_dir: &std::path::Path) -> Option<u16> {
    let path = base_dir.join("rsclaw.json5");
    let text = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = json5::from_str(&text).ok()?;
    value
        .get("gateway")
        .and_then(|g| g.get("port"))
        .and_then(|p| p.as_u64())
        .and_then(|p| u16::try_from(p).ok())
}

fn resolve_instance(cli: &Cli) -> (std::path::PathBuf, u16) {
    let home = dirs_next::home_dir().unwrap_or_default();

    // --base-dir replaces ~/.rsclaw as the root, then --dev/--profile append
    // suffix.
    let root = cli
        .base_dir
        .as_ref()
        .map(|p| rsclaw_config::loader::expand_tilde_path_pub(p))
        .unwrap_or_else(rsclaw_config::loader::base_dir);

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

/// Top-level async dispatch. Called by `main.rs` inside the tokio runtime.
#[allow(clippy::large_futures)]
pub async fn run() -> Result<()> {
    if let Some(path) = std::env::var_os(rsclaw_store::LEGACY_REDB_UPGRADE_HELPER_ENV) {
        return rsclaw_store::run_legacy_redb_upgrade_helper(std::path::Path::new(&path));
    }

    // Handle -v and -version before clap (clap handles --version and -V)
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() == 2 && (raw_args[1] == "-v" || raw_args[1] == "-version") {
        let version = option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev");
        match option_env!("RSCLAW_BUILD_COMMIT") {
            Some(commit) if !commit.is_empty() && commit != "unknown" => {
                println!("rsclaw v{version} ({commit})")
            }
            _ => println!("rsclaw v{version}"),
        }
        return Ok(());
    }

    let cli = Cli::parse();

    // Initialise logging.
    init_tracing(&cli);

    // Apply --base-dir / --dev / --profile instance isolation (AGENTS.md §26).
    let (base_dir, auto_port) = resolve_instance(&cli);
    if cli.base_dir.is_some() || cli.dev || cli.profile.is_some() {
        let label = cli
            .base_dir
            .as_deref()
            .unwrap_or_else(|| cli.profile.as_deref().unwrap_or("dev"));
        // Config wins: if the profile's rsclaw.json5 explicitly sets
        // gateway.port, that's the authoritative port for both the
        // gateway process and any CLI subcommand (channels pair, agents
        // bind, etc.) that talks to it via 127.0.0.1:<port>. The
        // auto-computed offset is only the FALLBACK for profiles that
        // never customized their port. Without this, binding a port in
        // config silently lost to the CLI flag and CLI tools dialed the
        // wrong port — reported as 'pair errors with port 18888'.
        let port = read_config_port(&base_dir).unwrap_or(auto_port);
        println!(
            "profile: {label}  base: {}  port: {port}",
            base_dir.display()
        );
        // SAFETY: called before tokio runtime starts, single-threaded at this point
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
        Command::Env(sub) => cmd_env(sub).await,
        Command::Gateway(sub) => cmd_gateway(sub).await,
        Command::Start => cmd_gateway(rsclaw_cli::GatewayCommand::Start).await,
        Command::Stop => cmd_gateway(rsclaw_cli::GatewayCommand::Stop).await,
        Command::Restart => cmd_gateway(rsclaw_cli::GatewayCommand::Restart).await,
        Command::Reload { scope } => {
            cmd_gateway(rsclaw_cli::GatewayCommand::Reload { scope }).await
        }
        Command::Channels(sub) => cmd_channels(sub).await,
        Command::Agents(sub) => cmd_agents(sub).await,
        Command::Models(sub) => cmd_models(sub).await,
        Command::Skills(sub) => cmd_skills(sub).await,
        Command::Plugins(sub) => cmd_plugins(sub).await,
        Command::Message(sub) => cmd_message(sub).await,
        Command::Kb(sub) => cmd_kb(sub, base_dir.join("kb")).await,
        Command::Image(sub) => cmd_image(sub).await,
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
        Command::Update(wrapper) | Command::Upgrade(wrapper) => {
            let sub = wrapper
                .cmd
                .unwrap_or(rsclaw_cli::UpdateCommand::Run(wrapper.run_args));
            cmd_update(sub).await
        }
        Command::Pairing(sub) => cmd_pairing(sub).await,
        Command::Approvals(sub) => cmd_approvals(sub).await,
        Command::Devices(sub) => cmd_devices(sub).await,
        Command::Directory(sub) => cmd_directory(sub).await,
        Command::Anycli(sub) => cmd_anycli(sub).await,
        Command::Browser(sub) => cmd_browser(sub).await,
        Command::Dns(sub) => cmd_dns(sub).await,
        Command::AgentTurn(args) => cmd_agent_turn(args).await,
        Command::Completion(args) => cmd_completion(args).await,
        Command::Dashboard { no_open } => cmd_dashboard(no_open).await,
        Command::Daemon(sub) => cmd_daemon(sub).await,
        Command::Docs { query } => cmd_docs(query).await,
        Command::Qr(args) => cmd_qr(args).await,
        Command::Uninstall(args) => cmd_uninstall(args).await,
        Command::Webhooks(sub) => cmd_webhooks(sub).await,
        Command::Watch(args) => cmd_watch(args).await,
        Command::Debug(sub) => cmd_debug(sub).await,
    }
}

// ---------------------------------------------------------------------------
// Pairing command
// ---------------------------------------------------------------------------

async fn cmd_pairing(sub: rsclaw_cli::PairingCommand) -> Result<()> {
    match sub {
        rsclaw_cli::PairingCommand::Approve { code } => {
            // Try to call running gateway API.
            let config = rsclaw_config::load().ok();
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
        rsclaw_cli::PairingCommand::Revoke { channel, peer } => {
            Box::pin(cmd::channels::cmd_channels(
                rsclaw_cli::ChannelsCommand::Unpair { channel, peer },
            ))
            .await?;
        }
        rsclaw_cli::PairingCommand::List => {
            Box::pin(cmd::channels::cmd_channels(
                rsclaw_cli::ChannelsCommand::Paired { channel: None },
            ))
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tracing initialisation
// ---------------------------------------------------------------------------

/// Resolve the gateway auth token: explicit `--token` > config > env var.
#[allow(dead_code)]
fn resolve_gateway_token(explicit: Option<String>) -> String {
    if let Some(t) = explicit {
        return t;
    }
    // Try loading from config file.
    if let Some(path) = rsclaw_config::loader::detect_config_path() {
        if let Ok(cfg) = rsclaw_config::loader::load_json5(&path) {
            if let Some(t) = cfg
                .gateway
                .as_ref()
                .and_then(|g| g.auth.as_ref())
                .and_then(|a| a.token.as_ref())
                .and_then(|s| s.as_plain())
                .map(str::to_owned)
            {
                return t;
            }
        }
    }
    std::env::var("RSCLAW_AUTH_TOKEN").unwrap_or_default()
}

fn init_tracing(cli: &Cli) {
    // Only `gateway run` gets info-level logs by default.
    // All other CLI commands default to warn (silent) unless overridden.
    let is_gateway_run = matches!(
        &cli.command,
        Command::Gateway(rsclaw_cli::GatewayCommand::Run(_))
    );
    let default_level =
        cli.log_level
            .as_deref()
            .unwrap_or(if is_gateway_run { "info" } else { "warn" });
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let timer = tracing_subscriber::fmt::time::ChronoLocal::rfc_3339();

    if cli.json {
        tracing_subscriber::fmt()
            .json()
            .with_timer(timer.clone())
            .with_env_filter(filter)
            .init();
    } else if cli.no_color {
        tracing_subscriber::fmt()
            .with_timer(timer)
            .with_env_filter(filter)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_timer(timer)
            .with_env_filter(filter)
            .init();
    }
}

/// Ensure the rustls TLS crypto provider is installed for all lib tests.
#[cfg(test)]
#[ctor::ctor]
fn init_test_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
