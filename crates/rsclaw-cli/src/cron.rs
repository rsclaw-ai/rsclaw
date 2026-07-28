use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum CronCommand {
    Status,
    List,
    Add(CronAddArgs),
    /// Edit a cron job. Pass --schedule / --message / --agent to patch inline;
    /// pass only the ID to open the raw file in $EDITOR.
    Edit(CronEditArgs),
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

#[derive(Args, Debug)]
pub struct CronEditArgs {
    /// Job id to edit.
    pub id: String,

    /// New cron schedule (5-field expression).
    #[arg(long)]
    pub schedule: Option<String>,

    /// New message text.
    #[arg(long)]
    pub message: Option<String>,

    /// New agent id.
    #[arg(long)]
    pub agent: Option<String>,

    /// Enable the job.
    #[arg(long, conflicts_with = "disable")]
    pub enable: bool,

    /// Disable the job.
    #[arg(long, conflicts_with = "enable")]
    pub disable: bool,
}
