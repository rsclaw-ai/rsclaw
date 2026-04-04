use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum SecretsCommand {
    Reload,
    Audit,
    Configure,
    Apply(SecretsApplyArgs),
}

#[derive(Args, Debug)]
pub struct SecretsApplyArgs {
    #[arg(long)]
    pub from: String,
    #[arg(long)]
    pub dry_run: bool,
}
