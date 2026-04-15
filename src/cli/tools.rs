use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ToolsCommand {
    /// Install a tool (chrome, ffmpeg, node, python, opencode, claude-code, all).
    Install {
        /// Tool name: chrome, ffmpeg, node, python, opencode, claude-code, all
        name: String,
        /// Force reinstall even if already installed or in PATH.
        #[arg(long, short)]
        force: bool,
    },
    /// List installed tools and their versions.
    List,
    /// Check which tools are available / missing.
    Status,
}
