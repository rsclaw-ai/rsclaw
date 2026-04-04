use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum SecurityCommand {
    Audit(SecurityAuditArgs),
}

#[derive(Args, Debug)]
pub struct SecurityAuditArgs {
    #[arg(long)]
    pub deep: bool,
    #[arg(long)]
    pub fix: bool,
    /// Output audit results in JSON format.
    #[arg(long)]
    pub json: bool,
    /// Bearer token for remote gateway audit.
    #[arg(long)]
    pub token: Option<String>,
    /// Password for remote gateway audit.
    #[arg(long)]
    pub password: Option<String>,
}
