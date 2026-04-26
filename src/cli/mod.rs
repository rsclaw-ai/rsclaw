//! CLI command tree -- mirrors the OpenClaw command surface.
//! Implemented with `clap` derive macros.

use clap::{Parser, Subcommand};

pub mod acp;
pub mod agent_turn;
pub mod agents;
pub mod anycli;
pub mod browser;
pub mod approvals;
pub mod channels;
pub mod completion;
pub mod config;
pub mod cron;
pub mod daemon;
pub mod devices;
pub mod directory;
pub mod dns;
pub mod doctor;
pub mod gateway;
pub mod hooks;
pub mod memory;
pub mod message;
pub mod migrate;
pub mod models;
pub mod ops;
pub mod plugins;
pub mod qr;
pub mod sandbox;
pub mod secrets;
pub mod security;
pub mod sessions;
pub mod setup;
pub mod skills;
pub mod system;
pub mod tools;
pub mod webhooks;

pub use acp::AcpCommand;
pub use agent_turn::AgentTurnArgs;
pub use agents::{AgentsCommand, BindArgs};
pub use anycli::AnycliCommand;
pub use browser::BrowserCommand;
pub use approvals::ApprovalsCommand;
pub use channels::ChannelsCommand;
pub use completion::CompletionArgs;
pub use config::ConfigCommand;
pub use cron::{CronAddArgs, CronCommand};
pub use daemon::DaemonCommand;
pub use devices::DevicesCommand;
pub use directory::DirectoryCommand;
pub use dns::DnsCommand;
pub use doctor::DoctorArgs;
pub use gateway::{GatewayCommand, GatewayRunArgs};
pub use hooks::HooksCommand;
pub use memory::{MemoryCommand, MemoryIndexArgs, MemorySearchArgs, MemoryStatusArgs};
pub use message::MessageCommand;
pub use migrate::MigrateArgs;
pub use models::{
    AliasesCommand, AuthOrderCommand, FallbacksCommand, ImageFallbacksCommand, ModelsAuthCommand,
    ModelsCommand,
};
pub use ops::{
    BackupCommand, BackupCreateArgs, HealthArgs, LogsArgs, ResetArgs, StatusArgs, TuiArgs,
    UpdateArgs, UpdateCommand,
};
pub use plugins::PluginsCommand;
pub use qr::QrArgs;
pub use sandbox::SandboxCommand;
pub use secrets::{SecretsApplyArgs, SecretsCommand};
pub use security::{SecurityAuditArgs, SecurityCommand};
pub use sessions::{CleanupArgs, SessionsCommand, SessionsListArgs};
pub use setup::{ConfigureArgs, OnboardArgs, SetupArgs};
pub use skills::SkillsCommand;
pub use system::{HeartbeatCommand, SystemCommand};
pub use tools::ToolsCommand;
pub use webhooks::WebhooksCommand;

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "rsclaw",
    version = option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"),
    about = "AI Agent Engine Compatible with OpenClaw",
    long_about = None,
)]
pub struct Cli {
    /// Isolate state to ~/.rsclaw-dev, port offset applied.
    #[arg(long, global = true)]
    pub dev: bool,

    /// Isolate state to ~/.rsclaw-<n>.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Override the base directory (default: ~/.rsclaw).
    /// Takes precedence over --dev and --profile.
    #[arg(long, global = true, value_name = "PATH")]
    pub base_dir: Option<String>,

    /// Override the config file path (default: auto-detected).
    #[arg(long, global = true, value_name = "PATH")]
    pub config_path: Option<String>,

    /// Disable ANSI colour output.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Machine-readable JSON output (disables styling).
    #[arg(long, global = true)]
    pub json: bool,

    /// Run inside a container (Podman/Docker). Currently prints a warning.
    #[arg(long, global = true, value_name = "NAME")]
    pub container: Option<String>,

    /// Override the global log level (trace, debug, info, warn, error).
    #[arg(long, global = true, value_name = "LEVEL")]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

// ---------------------------------------------------------------------------
// Top-level commands
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialise config and workspace (non-interactive).
    Setup(SetupArgs),

    /// Interactive onboarding wizard.
    Onboard(OnboardArgs),

    /// Interactive configuration wizard.
    Configure(ConfigureArgs),

    /// Config management sub-commands.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Diagnose and optionally fix configuration issues.
    Doctor(DoctorArgs),

    /// Gateway lifecycle sub-commands.
    #[command(subcommand)]
    Gateway(GatewayCommand),

    /// Alias for `gateway start`.
    #[command(hide = true)]
    Start,

    /// Alias for `gateway stop`.
    #[command(hide = true)]
    Stop,

    /// Alias for `gateway restart`.
    #[command(hide = true)]
    Restart,

    /// Channel management sub-commands.
    #[command(subcommand)]
    Channels(ChannelsCommand),

    /// Agent management sub-commands.
    #[command(subcommand)]
    Agents(AgentsCommand),

    /// Model management sub-commands.
    #[command(subcommand)]
    Models(ModelsCommand),

    /// Skill management.
    #[command(subcommand)]
    Skills(SkillsCommand),

    /// Plugin management.
    #[command(subcommand)]
    Plugins(PluginsCommand),

    /// Memory management.
    #[command(subcommand)]
    Memory(MemoryCommand),

    /// Session management.
    #[command(subcommand)]
    Sessions(SessionsCommand),

    /// Cron job management.
    #[command(subcommand)]
    Cron(CronCommand),

    /// Webhook hook management.
    #[command(subcommand)]
    Hooks(HooksCommand),

    /// System utilities.
    #[command(subcommand)]
    System(SystemCommand),

    /// External tool management (chrome, ffmpeg, opencode, etc.).
    #[command(subcommand)]
    Tools(ToolsCommand),

    /// Secrets management.
    #[command(subcommand)]
    Secrets(SecretsCommand),

    /// Security audit.
    #[command(subcommand)]
    Security(SecurityCommand),

    /// Sandbox management.
    #[command(subcommand)]
    Sandbox(SandboxCommand),

    /// Tail gateway logs.
    Logs(LogsArgs),

    /// Show overall status.
    Status(StatusArgs),

    /// Health check.
    Health(HealthArgs),

    /// Send, read, and manage messages across chat channels.
    #[command(subcommand)]
    Message(MessageCommand),

    /// Terminal UI (TUI).
    Tui(TuiArgs),

    /// System tray icon (requires --features tray).
    Tray,

    /// Backup management.
    #[command(subcommand)]
    Backup(BackupCommand),

    /// Reset state.
    Reset(ResetArgs),

    /// Update rsclaw binary.
    #[command(subcommand)]
    Update(UpdateCommand),

    /// Alias for `update`.
    #[command(subcommand)]
    Upgrade(UpdateCommand),

    /// DM pairing management.
    #[command(subcommand)]
    Pairing(PairingCommand),

    /// ACP protocol commands - control coding agents
    #[command(subcommand)]
    Acp(AcpCommand),


    /// Manage exec approvals.
    #[command(subcommand)]
    Approvals(ApprovalsCommand),

    /// Device pairing and token management.
    #[command(subcommand)]
    Devices(DevicesCommand),

    /// Contact/group ID lookup via gateway directory API.
    #[command(subcommand)]
    Directory(DirectoryCommand),

    /// Extract structured data from websites using declarative adapters.
    #[command(subcommand)]
    Anycli(AnycliCommand),

    /// Control a web browser directly (open, snapshot, click, fill, etc.).
    #[command(subcommand)]
    Browser(BrowserCommand),

    /// DNS helpers for Tailscale wide-area discovery.
    #[command(subcommand)]
    Dns(DnsCommand),

    /// Run a single agent turn (openclaw-compatible).
    #[command(name = "agent-turn")]
    AgentTurn(AgentTurnArgs),
    /// Generate shell completion scripts.
    Completion(CompletionArgs),

    /// Open the Control UI dashboard in a browser.
    Dashboard {
        /// Print the URL instead of opening a browser.
        #[arg(long)]
        no_open: bool,
    },

    /// Legacy gateway alias.
    #[command(subcommand)]
    Daemon(DaemonCommand),

    /// Search live documentation.
    Docs {
        /// Search query terms.
        query: Vec<String>,
    },

    /// Generate iOS pairing QR code.
    Qr(QrArgs),

    /// Migrate data from OpenClaw to rsclaw.
    Migrate(MigrateArgs),

    /// Uninstall service and remove data.
    Uninstall(UninstallArgs),

    /// Webhook helpers.
    #[command(subcommand)]
    Webhooks(WebhooksCommand),
}

#[derive(clap::Args, Debug)]
pub struct UninstallArgs {
    /// Remove the gateway system service.
    #[arg(long)]
    pub service: bool,

    /// Remove the ~/.rsclaw/ state directory.
    #[arg(long)]
    pub state: bool,

    /// Remove agent workspace directories.
    #[arg(long)]
    pub workspace: bool,

    /// Remove the rsclaw binary.
    #[arg(long)]
    pub app: bool,

    /// Remove everything (service + state + workspace + app).
    #[arg(long)]
    pub all: bool,

    /// Show what would be removed without doing it.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip confirmation prompt.
    #[arg(long)]
    pub yes: bool,

    /// Non-interactive mode (implies --yes).
    #[arg(long)]
    pub non_interactive: bool,
}

#[derive(Subcommand, Debug)]
pub enum PairingCommand {
    /// Approve a pairing code.
    Approve {
        /// The pairing code (e.g. "1234-5678").
        code: String,
    },
    /// Revoke an approved peer.
    Revoke {
        /// Channel name.
        #[arg(long)]
        channel: String,
        /// Peer ID.
        #[arg(long)]
        peer: String,
    },
    /// List all approved peers.
    List,
}
