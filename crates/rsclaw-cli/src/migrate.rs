//! CLI argument definitions for `rsclaw migrate`.

use clap::Args;

#[derive(Args, Debug)]
pub struct MigrateArgs {
    /// Migration mode: seamless, import, or fresh.
    #[arg(long, default_value = "import")]
    pub mode: String,

    /// Path to OpenClaw data directory (auto-detected if omitted).
    /// Searches ~/.openclaw/ and ~/bak.openclaw/ by default.
    #[arg(long, value_name = "PATH")]
    pub openclaw_dir: Option<String>,

    /// Show what would be done without actually doing it.
    #[arg(long)]
    pub dry_run: bool,
}
