use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum MemoryCommand {
    Status(MemoryStatusArgs),
    Index(MemoryIndexArgs),
    Search(MemorySearchArgs),
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
}
