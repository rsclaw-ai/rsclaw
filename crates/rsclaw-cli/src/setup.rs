use clap::Args;

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Run interactive wizard (equivalent to `onboard`).
    #[arg(long)]
    pub wizard: bool,
    /// Setup mode (e.g. "local", "remote", "hybrid").
    #[arg(long)]
    pub mode: Option<String>,
    /// Skip interactive prompts.
    #[arg(long)]
    pub non_interactive: bool,
    /// Remote gateway URL to connect to.
    #[arg(long)]
    pub remote_url: Option<String>,
    /// Token for remote gateway authentication.
    #[arg(long)]
    pub remote_token: Option<String>,
    /// Workspace directory path.
    #[arg(long)]
    pub workspace: Option<String>,
}

#[derive(Args, Debug, Default)]
pub struct OnboardArgs {
    /// Skip interactive prompts.
    #[arg(long)]
    pub non_interactive: bool,
    /// Onboarding flow (e.g. "quick", "full", "minimal").
    #[arg(long)]
    pub flow: Option<String>,
    /// Setup mode (e.g. "local", "remote", "hybrid").
    #[arg(long)]
    pub mode: Option<String>,
    /// Gateway port to use.
    #[arg(long)]
    pub gateway_port: Option<u16>,
    /// Gateway authentication token.
    #[arg(long)]
    pub gateway_token: Option<String>,
    /// Skip channel configuration.
    #[arg(long)]
    pub skip_channels: bool,
    /// Skip skill configuration.
    #[arg(long)]
    pub skip_skills: bool,
    /// Skip daemon setup.
    #[arg(long)]
    pub skip_daemon: bool,
    /// Output in JSON format.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Default)]
pub struct ConfigureArgs {
    /// Limit configuration to specific section(s).
    #[arg(long)]
    pub section: Vec<String>,
}
