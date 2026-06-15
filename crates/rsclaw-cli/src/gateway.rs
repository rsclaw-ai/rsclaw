use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum GatewayCommand {
    /// Start gateway as a background daemon.
    Start,
    /// Stop the running gateway.
    Stop,
    /// Restart the gateway.
    Restart,
    /// Run gateway in the foreground (for systemd/launchd).
    Run(GatewayRunArgs),
    /// Show gateway status.
    Status,
    /// Health check endpoint probe.
    Health,
    /// Probe gateway connectivity.
    Probe,
    /// Register as a system service (systemd / launchd).
    Install,
    /// Remove system service registration.
    Uninstall,
    /// Call a gateway RPC method.
    Call { method: String, args: Vec<String> },
    /// Discover gateways on the local network.
    Discover,
    /// Show gateway usage and cost statistics.
    UsageCost,
}

#[derive(Args, Debug, Default)]
pub struct GatewayRunArgs {
    /// Port to listen on (overrides config).
    #[arg(long)]
    pub port: Option<u16>,
    /// Bind address (e.g. "0.0.0.0", "loopback", "127.0.0.1").
    #[arg(long)]
    pub bind: Option<String>,
    /// Auth mode (e.g. "token", "password", "none").
    #[arg(long)]
    pub auth: Option<String>,
    /// Bearer token for gateway authentication.
    #[arg(long)]
    pub token: Option<String>,
    /// Password for gateway authentication.
    #[arg(long)]
    pub password: Option<String>,
    /// Force start even if another instance is running.
    #[arg(long)]
    pub force: bool,
    /// Enable verbose logging.
    #[arg(long)]
    pub verbose: bool,
    /// Enable compact log format.
    #[arg(long)]
    pub compact: bool,
    /// Enable WebSocket debug logging.
    #[arg(long)]
    pub ws_log: bool,
}
