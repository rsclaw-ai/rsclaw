//! `rsclaw watch …` — stream a file / SSE / shell source to stdout.
//!
//! Reuses the same `/watch` parser used by the chat slash command, but
//! delivers each event to the local terminal instead of a chat channel.

use clap::Args;

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// The watch body. Same grammar as the `/watch` chat command:
    ///
    ///   rsclaw watch sse ${ASTOCK}
    ///   rsclaw watch file ~/.rsclaw/var/logs/gateway.log --grep ERR
    ///   rsclaw watch shell tail -f /var/log/app.log
    ///
    /// Auto-detects `sse` (http/https URL) or `file` (path-like first
    /// token); for `shell` the keyword is required. `${VAR}` references
    /// in the source / headers are resolved from the calling shell's
    /// environment.
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    pub body: Vec<String>,
}
