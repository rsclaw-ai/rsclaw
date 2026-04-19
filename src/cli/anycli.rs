use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum AnycliCommand {
    /// Run an adapter command to extract data from a website.
    Run {
        /// Adapter name (e.g., "hackernews", "bilibili").
        adapter: String,
        /// Command name (e.g., "top", "search").
        command: String,
        /// Parameters as key=value pairs (e.g., limit=10 query="rust").
        #[arg(trailing_var_arg = true)]
        params: Vec<String>,
        /// Output format: json, table, csv, markdown.
        #[arg(long, short, default_value = "json")]
        format: String,
    },
    /// List all available adapters.
    List,
    /// Show details of a specific adapter.
    Info {
        /// Adapter name.
        adapter: String,
    },
    /// Search adapters in the community hub.
    Search {
        /// Search query.
        query: String,
    },
    /// Install an adapter from the community hub.
    Install {
        /// Adapter name to install.
        name: String,
    },
    /// Update all installed adapters from the hub.
    Update,
}
