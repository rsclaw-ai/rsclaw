use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum SessionsCommand {
    List(SessionsListArgs),
    Cleanup(CleanupArgs),
    /// Export session messages as JSONL for fine-tuning / calibration.
    Export(ExportArgs),
}

#[derive(Args, Debug)]
pub struct SessionsListArgs {
    /// Show only active sessions.
    #[arg(long)]
    pub active: bool,
    /// Filter by agent ID.
    #[arg(long)]
    pub agent: Option<String>,
    /// Show sessions from all agents.
    #[arg(long)]
    pub all_agents: bool,
    /// Output in JSON format.
    #[arg(long)]
    pub json: bool,
    /// Filter by session store.
    #[arg(long)]
    pub store: Option<String>,
    /// Enable verbose output.
    #[arg(long)]
    pub verbose: bool,
}

#[derive(Args, Debug)]
pub struct CleanupArgs {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub enforce: bool,
    #[arg(long)]
    pub active_key: Option<String>,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Export a specific session key. If omitted, exports all sessions.
    #[arg(long)]
    pub session: Option<String>,
    /// Output file path (default: stdout).
    #[arg(long, short)]
    pub output: Option<String>,
    /// Maximum number of sessions to export.
    #[arg(long, default_value = "500")]
    pub limit: usize,
}
