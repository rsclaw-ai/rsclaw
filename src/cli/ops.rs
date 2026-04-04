use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct LogsArgs {
    #[arg(long)]
    pub follow: bool,
    #[arg(long)]
    pub level: Option<String>,
    /// Output logs in JSON format.
    #[arg(long)]
    pub json: bool,
    /// Maximum number of log lines to show.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Maximum bytes to read from log file.
    #[arg(long)]
    pub max_bytes: Option<usize>,
    /// Polling interval in milliseconds for --follow mode.
    #[arg(long)]
    pub interval: Option<u64>,
    /// Show timestamps in local time.
    #[arg(long)]
    pub local_time: bool,
    /// Disable colour/styling in output.
    #[arg(long)]
    pub plain: bool,
    /// Timeout in seconds for log streaming.
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Bearer token for remote gateway log access.
    #[arg(long)]
    pub token: Option<String>,
    /// Remote gateway URL for log streaming.
    #[arg(long)]
    pub url: Option<String>,
}

#[derive(Args, Debug)]
pub struct ResetArgs {
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,
    /// Skip confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Show what would be deleted without actually deleting.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip interactive prompts.
    #[arg(long)]
    pub non_interactive: bool,
}

#[derive(Subcommand, Debug)]
pub enum BackupCommand {
    Create(BackupCreateArgs),
    Verify { file: String },
}

#[derive(Args, Debug)]
pub struct BackupCreateArgs {
    /// Include session transcripts in the backup.
    #[arg(long)]
    pub include_sessions: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Show all status details.
    #[arg(long)]
    pub all: bool,
    /// Run deep status checks.
    #[arg(long)]
    pub deep: bool,
    /// Output status in JSON format.
    #[arg(long)]
    pub json: bool,
    /// Enable verbose output.
    #[arg(long)]
    pub verbose: bool,
    /// Timeout in seconds for status checks.
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Include usage statistics.
    #[arg(long)]
    pub usage: bool,
}

#[derive(Args, Debug)]
pub struct HealthArgs {
    /// Output health check result in JSON format.
    #[arg(long)]
    pub json: bool,
    /// Enable verbose output.
    #[arg(long)]
    pub verbose: bool,
    /// Timeout in seconds for health check.
    #[arg(long)]
    pub timeout: Option<u64>,
}

#[derive(Args, Debug)]
pub struct TuiArgs {
    /// Gateway URL to connect to.
    #[arg(long)]
    pub url: Option<String>,
    /// Bearer token for gateway authentication.
    #[arg(long)]
    pub token: Option<String>,
    /// Password for gateway authentication.
    #[arg(long)]
    pub password: Option<String>,
    /// Session ID to resume.
    #[arg(long)]
    pub session: Option<String>,
    /// Initial message to send on launch.
    #[arg(long)]
    pub message: Option<String>,
    /// Deliver the message immediately without confirmation.
    #[arg(long)]
    pub deliver: bool,
    /// Enable thinking/reasoning display.
    #[arg(long)]
    pub thinking: bool,
    /// Maximum number of history entries to load.
    #[arg(long)]
    pub history_limit: Option<usize>,
    /// Timeout in milliseconds for gateway requests.
    #[arg(long)]
    pub timeout_ms: Option<u64>,
}

#[derive(Subcommand, Debug)]
pub enum UpdateCommand {
    /// Run the update process (default).
    Run(UpdateArgs),
    /// Show current update status.
    Status,
    /// Interactive update wizard.
    Wizard,
}

#[derive(Args, Debug, Default)]
pub struct UpdateArgs {
    /// Update channel (stable, beta, nightly).
    #[arg(long)]
    pub channel: Option<String>,
    /// Specific version tag to install.
    #[arg(long)]
    pub tag: Option<String>,
    /// Show what would be done without making changes.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip confirmation prompts.
    #[arg(long)]
    pub yes: bool,
    /// Output in JSON format.
    #[arg(long)]
    pub json: bool,
    /// Do not restart the gateway after update.
    #[arg(long)]
    pub no_restart: bool,
    /// Timeout in seconds for download.
    #[arg(long)]
    pub timeout: Option<u64>,
}
