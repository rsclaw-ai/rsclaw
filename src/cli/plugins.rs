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
}
