//! `rsclaw env …` — manage the auto-managed `.env` file.

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum EnvCommand {
    /// Snapshot the current shell env into `.env` for every var
    /// referenced in `rsclaw.json5`. Adds missing entries; updates
    /// values that drifted from the shell ("shell wins on diff").
    /// Vars not present in the shell are reported but not removed
    /// from `.env`.
    Sync(EnvSyncArgs),
    /// Show every var referenced in `rsclaw.json5` alongside its
    /// current resolution status (shell / .env / shell-rc / missing).
    List,
}

#[derive(clap::Args, Debug)]
pub struct EnvSyncArgs {
    /// Print what would change without writing `.env`. Exit 0 when
    /// no changes, 0 with output when there are pending changes.
    #[arg(long)]
    pub dry_run: bool,
    /// Overwrite every entry in `.env` with the current shell value,
    /// even when the shell didn't set the var (results in empty
    /// values). Default behaviour is "additive": shell wins on
    /// diff, missing vars stay as-is in `.env`.
    #[arg(long)]
    pub force: bool,
}
