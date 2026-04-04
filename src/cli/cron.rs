use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum CronCommand {
    Status,
    List,
    Add(CronAddArgs),
    Edit { id: String },
    Rm { id: String },
    Enable { id: String },
    Disable { id: String },
    Runs { id: String },
    Run { id: String },
}

#[derive(Args, Debug)]
pub struct CronAddArgs {
    #[arg(long)]
    pub schedule: String,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub message: String,
}
