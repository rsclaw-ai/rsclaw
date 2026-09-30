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
    /// Also read from `RSCLAW_AUTH_TOKEN` so the secret need not appear in
    /// argv (visible in `ps` / shell history).
    #[arg(long, env = "RSCLAW_AUTH_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// Password for remote gateway audit.
    /// Also read from `RSCLAW_GATEWAY_PASSWORD` so the secret need not appear in
    /// argv (visible in `ps` / shell history).
    #[arg(long, env = "RSCLAW_GATEWAY_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,
}
