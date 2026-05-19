//! Knowledge-base CLI subcommands. Spec §5 V1.

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum KbCommand {
    /// Add a document (file path or URL) to the knowledge base.
    Add {
        /// File path or URL (http://, https://).
        path_or_url: String,
        #[arg(long)]
        tags: Vec<String>,
    },
    /// List documents.
    Ls {
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        source_kind: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Remove a document by id (tombstone).
    Rm {
        doc_id: String,
        #[arg(long)]
        yes: bool,
    },
    /// Search the knowledge base.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 8)]
        k: usize,
    },
    /// Show a chunk or doc by id.
    Show { id: String },
    /// Update document visibility.
    Visibility {
        doc_id: String,
        /// One of: global | private | agent:<id> | channel:<id>
        visibility: String,
    },
    /// Run a compactor tick (orphan cleanup + ledger advance).
    Compact,
    /// Show kb stats (doc/chunk counts, disk usage).
    Stats,
}
