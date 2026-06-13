use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DevicesCommand {
    /// List paired devices.
    List,
    /// Approve a pending device.
    Approve { id: String },
    /// Reject a pending device.
    Reject { id: String },
    /// Remove a paired device.
    Remove { id: String },
    /// Revoke a device token by role.
    Revoke {
        #[arg(long)]
        role: String,
    },
    /// Rotate a device token by role.
    Rotate {
        #[arg(long)]
        role: String,
    },
    /// Clear all paired devices.
    Clear,
}
