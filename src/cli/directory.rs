use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DirectoryCommand {
    /// Look up own identity on a channel.
    #[command(name = "self")]
    Self_ {
        #[arg(long)]
        channel: String,
    },
    /// Peer lookup.
    #[command(subcommand)]
    Peers(PeersCommand),
    /// Group lookup.
    #[command(subcommand)]
    Groups(GroupsCommand),
}

#[derive(Subcommand, Debug)]
pub enum PeersCommand {
    /// List peers on a channel.
    List {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        query: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum GroupsCommand {
    /// List groups on a channel.
    List {
        #[arg(long)]
        channel: String,
    },
    /// List members of a group.
    Members {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        group_id: String,
    },
}
