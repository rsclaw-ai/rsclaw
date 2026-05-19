//! Knowledge-base CLI subcommands. Spec §5 V1.

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum KbCommand {
    /// Add a document (file path, directory, or URL) to the knowledge base.
    Add {
        /// File path, directory, or URL (http://, https://).
        path_or_url: String,
        #[arg(long)]
        tags: Vec<String>,
        /// When `path_or_url` is a directory, recursively ingest
        /// every file matching `--ext` (default: md,txt,html,pdf).
        #[arg(long)]
        recursive: bool,
        /// Comma-separated extensions to ingest in recursive mode.
        #[arg(long, default_value = "md,txt,html,pdf")]
        ext: String,
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
    /// Remove a document by id, or all docs matching a tag.
    Rm {
        /// Doc id to tombstone. Omit when using --tag.
        doc_id: Option<String>,
        /// Tombstone every Active doc with this tag (bulk delete).
        #[arg(long)]
        tag: Option<String>,
        /// Skip the confirmation guard (required for tombstone).
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
    /// Export a doc's markdown body to a path.
    Export {
        doc_id: String,
        #[arg(long)]
        to: std::path::PathBuf,
    },
}
