use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ToolsCommand {
    /// Install a tool (chromium, ffmpeg, whisper-cpp, node, all).
    Install {
        /// Tool name: chromium, ffmpeg, whisper-cpp, node, all
        name: String,
    },
    /// List installed tools and their versions.
    List,
    /// Check which tools are available / missing.
    Status,
}
