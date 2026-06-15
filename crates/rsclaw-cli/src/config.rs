use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Get a config value by dotted key path.
    Get {
        key: String,
        /// Limit to a specific config section.
        #[arg(long)]
        section: Option<String>,
    },
    /// Set a config value.
    Set {
        key: String,
        value: String,
        /// Limit to a specific config section.
        #[arg(long)]
        section: Option<String>,
    },
    /// Unset / remove a config key.
    Unset {
        key: String,
        /// Limit to a specific config section.
        #[arg(long)]
        section: Option<String>,
    },
    /// Print the path of the current config file.
    File,
    /// Validate the current config.
    Validate,
}
