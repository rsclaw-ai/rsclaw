use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ApprovalsCommand {
    /// Show current exec approvals.
    Get,
    /// Set exec approvals from a JSON file.
    Set { file: String },
    /// Manage the exec allowlist.
    #[command(subcommand)]
    Allowlist(AllowlistCommand),
}

#[derive(Subcommand, Debug)]
pub enum AllowlistCommand {
    /// Add a pattern to the allowlist for an agent.
    Add {
        agent: String,
        #[arg(long)]
        pattern: String,
    },
    /// Remove a pattern from the allowlist for an agent.
    Remove {
        agent: String,
        #[arg(long)]
        pattern: String,
    },
}
