use clap::Args;

/// Run a single agent turn (openclaw-compatible `agent` command).
#[derive(Args, Debug)]
pub struct AgentTurnArgs {
    /// Target recipient (user or channel).
    #[arg(short = 't', long)]
    pub to: Option<String>,

    /// Message to send.
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Deliver the reply to the target channel via the gateway.
    #[arg(long)]
    pub deliver: bool,

    /// Thinking mode (e.g. "extended", "basic").
    #[arg(long)]
    pub thinking: Option<String>,

    /// Use embedded local provider instead of the gateway.
    #[arg(long)]
    pub local: bool,

    /// Channel to use.
    #[arg(long)]
    pub channel: Option<String>,

    /// Agent ID to invoke.
    #[arg(long)]
    pub agent: Option<String>,

    /// Session ID to continue.
    #[arg(long)]
    pub session_id: Option<String>,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,

    /// Request timeout in seconds.
    #[arg(long)]
    pub timeout: Option<u64>,

    /// Verbose logging level.
    #[arg(long)]
    pub verbose: Option<String>,

    /// Reply to a specific message ID.
    #[arg(long)]
    pub reply_to: Option<String>,

    /// Reply channel override.
    #[arg(long)]
    pub reply_channel: Option<String>,

    /// Reply account override.
    #[arg(long)]
    pub reply_account: Option<String>,
}
