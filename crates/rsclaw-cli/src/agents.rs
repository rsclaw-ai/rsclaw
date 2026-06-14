use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum AgentsCommand {
    List,
    Add {
        name: String,
    },
    Delete {
        id: String,
    },
    Bindings,
    Bind(BindArgs),
    Unbind {
        binding_id: String,
    },
    /// Update agent identity fields (name, theme, emoji, avatar).
    SetIdentity {
        #[arg(long)]
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        theme: Option<String>,
        #[arg(long)]
        emoji: Option<String>,
        #[arg(long)]
        avatar: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct BindArgs {
    pub agent_id: String,
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub peer_id: Option<String>,
    #[arg(long)]
    pub group_id: Option<String>,
    #[arg(long)]
    pub priority: Option<i32>,
}
