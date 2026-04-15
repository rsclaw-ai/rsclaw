use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ToolsCommand {
    /// Install a tool (chromium, ffmpeg, whisper-cpp, node, python, all).
    Install {
        /// Tool name: chromium, ffmpeg, whisper-cpp, node, python, all
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
