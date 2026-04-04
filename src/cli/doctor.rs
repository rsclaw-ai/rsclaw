use clap::Args;

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Automatically apply safe fixes.
    #[arg(long)]
    pub fix: bool,
    /// Do not prompt for confirmation.
    #[arg(long)]
    pub yes: bool,
    /// Run deep diagnostics (slower, more thorough).
    #[arg(long)]
    pub deep: bool,
    /// Attempt to repair detected issues.
    #[arg(long)]
    pub repair: bool,
    /// Force operations without confirmation.
    #[arg(long)]
    pub force: bool,
    /// Skip interactive prompts.
    #[arg(long)]
    pub non_interactive: bool,
    /// Generate a new gateway authentication token.
    #[arg(long)]
    pub generate_gateway_token: bool,
}
