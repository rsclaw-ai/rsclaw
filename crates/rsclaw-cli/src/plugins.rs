use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum PluginsCommand {
    List,
    Info {
        plugin: String,
    },
    Install {
        spec: String,
    },
    Enable {
        plugin: String,
    },
    Disable {
        plugin: String,
    },
    Doctor,
    /// Show plugin manifest and internal details.
    Inspect {
        plugin: String,
    },
    /// Open the plugin marketplace in a browser or print URL.
    Marketplace,
    /// Uninstall a plugin (remove directory and config entry).
    Uninstall {
        plugin: String,
    },
    /// Update a plugin (or all plugins if none specified).
    Update {
        plugin: Option<String>,
    },
    /// Describe a plugin's tool surface (name + description + parameters).
    /// Talks to the running gateway over HTTP; the gateway must be up.
    Describe {
        plugin: String,
    },
    /// Invoke a plugin tool via the running gateway. Args are passed as a
    /// JSON object via `--args`. The gateway must be up.
    Call {
        /// Tool reference in `plugin.tool` form, e.g. `jimeng.txt2img`.
        tool_ref: String,
        /// JSON object of arguments (default: `{}`).
        #[arg(long, default_value = "{}")]
        args: String,
    },
}
