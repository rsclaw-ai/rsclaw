use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ChannelsCommand {
    /// List configured channels and their status.
    List,
    /// Show channel health status.
    Status,
    /// Tail channel logs.
    Logs { channel: Option<String> },
    /// Enable a channel in config.
    Add { channel: String },
    /// Remove a channel from config.
    Remove { channel: String },
    /// Interactive login for a channel (QR scan, OAuth, etc.).
    Login {
        channel: String,
        /// Quiet mode: only output QR image path (for desktop app integration).
        #[arg(long, short)]
        quiet: bool,
    },
    /// Remove stored credentials for a channel.
    Logout { channel: String },
    /// Approve a pairing code (for dmPolicy=pairing).
    Pair {
        /// The pairing code to approve (e.g. "45GA-KP42").
        code: String,
        /// Also add the approved peer to `gateway.owners` (full owner access:
        /// shell, file writes, cron, local slash commands). Use only for your
        /// own account.
        #[arg(long)]
        owner: bool,
    },
    /// Manage owner identities in `gateway.owners` (`"<channel>:<peer_id>"`).
    Owner {
        #[command(subcommand)]
        action: ChannelsOwnerCommand,
    },
    /// Revoke a previously approved peer.
    Unpair {
        /// Channel name (e.g. "telegram").
        #[arg(long)]
        channel: String,
        /// Peer ID to revoke.
        #[arg(long)]
        peer: String,
    },
    /// List approved peers for a channel.
    Paired {
        /// Channel name (e.g. "telegram").
        channel: Option<String>,
    },
    /// Show channel capabilities and feature support.
    Capabilities {
        #[arg(long)]
        channel: String,
    },
    /// Resolve a name to a channel-specific ID via gateway.
    Resolve {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        name: String,
    },
}

/// `rsclaw channels owner ...` actions. Each edits `gateway.owners` in the
/// config file; a running gateway picks the change up via its config watcher.
#[derive(Subcommand, Debug)]
pub enum ChannelsOwnerCommand {
    /// Grant owner access to a channel peer.
    Add {
        /// Channel name (e.g. "telegram", "feishu").
        channel: String,
        /// Peer ID on that channel.
        peer: String,
    },
    /// Revoke owner access from a channel peer.
    Remove {
        /// Channel name (e.g. "telegram", "feishu").
        channel: String,
        /// Peer ID on that channel.
        peer: String,
    },
    /// List configured owners, optionally filtered by channel.
    List {
        /// Channel name to filter by.
        channel: Option<String>,
    },
}
