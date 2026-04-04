use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum SessionsCommand {
    List(SessionsListArgs),
    Cleanup(CleanupArgs),
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
