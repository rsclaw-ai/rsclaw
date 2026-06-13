use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum MemoryCommand {
    Status(MemoryStatusArgs),
    Index(MemoryIndexArgs),
    Search(MemorySearchArgs),
    Save(MemorySaveArgs),
}

#[derive(Args, Debug)]
pub struct MemoryStatusArgs {
    /// Run deep analysis of memory store.
    #[arg(long)]
    pub deep: bool,
    /// Output in JSON format.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct MemoryIndexArgs {
    /// Force full re-index even if up to date.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct MemorySearchArgs {
    /// Search query.
    pub query: String,
    /// Maximum number of results to return.
    #[arg(long, default_value = "10")]
    pub max_results: usize,
    /// Output raw JSON instead of pretty-printed.
    #[arg(long)]
    pub json: bool,
}

/// `rsclaw memory save <text>` — persist a fact into the live store via
/// the gateway HTTP API. Write paths can't safely fall back to direct
/// redb because the gateway holds the exclusive write lock.
#[derive(Args, Debug)]
pub struct MemorySaveArgs {
    /// The fact / note text to remember.
    pub text: String,
    /// Logical scope (default "global"; use "agent:<id>" or
    /// "agent:<id>:<channel>" to scope to one agent/channel).
    #[arg(long)]
    pub scope: Option<String>,
    /// Document kind (default "fact"; common values: fact, note, summary).
    #[arg(long)]
    pub kind: Option<String>,
    /// Importance score in [0.0, 1.0] (default 0.7).
    #[arg(long)]
    pub importance: Option<f32>,
    /// Pin this fact — never decays, immune to tier demotion.
    #[arg(long)]
    pub pinned: bool,
    /// Repeatable tag for lifecycle tracking.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
    /// Output raw JSON instead of a status line.
    #[arg(long)]
    pub json: bool,
}
