use clap::Subcommand;

/// Legacy alias for `gateway` sub-commands.
#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Register as a system service.
    Install,
    /// Start the daemon.
    Start,
    /// Stop the daemon.
    Stop,
    /// Restart the daemon.
    Restart,
    /// Show daemon status.
    Status,
    /// Remove system service registration.
    Uninstall,
}
