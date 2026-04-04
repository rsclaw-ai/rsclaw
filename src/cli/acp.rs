use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum AcpCommand {
    #[command(name = "spawn", about = "Spawn a coding agent locally via ACP")]
    Spawn {
        #[arg(long, default_value = "opencode")]
        command: String,

        #[arg(long)]
        cwd: Option<String>,

        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    #[command(
        name = "connect",
        about = "Connect to a Gateway and spawn/control agent"
    )]
    Connect {
        #[arg(long, default_value = "ws://localhost:18888")]
        url: String,

        #[arg(long)]
        token: Option<String>,

        #[arg(long)]
        password: Option<String>,

        #[arg(long)]
        cwd: Option<String>,

        #[arg(long)]
        label: Option<String>,

        #[arg(long)]
        model: Option<String>,
    },

    #[command(name = "run", about = "Run a task with an agent (non-interactive)")]
    Run {
        #[arg(trailing_var_arg = true)]
        task: Vec<String>,

        #[arg(long)]
        session_id: Option<String>,

        #[arg(long)]
        cwd: Option<String>,

        #[arg(long, default_value = "opencode")]
        command: String,
    },

    #[command(name = "send", about = "Send a prompt to an existing agent")]
    Send {
        #[arg(long)]
        session_id: String,

        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
    },

    #[command(name = "list", about = "List agents on Gateway")]
    List {
        #[arg(long, default_value = "ws://localhost:18888")]
        url: String,

        #[arg(long)]
        token: Option<String>,
    },

    #[command(name = "kill", about = "Kill an agent on Gateway")]
    Kill {
        #[arg(long, default_value = "ws://localhost:18888")]
        url: String,

        #[arg(long)]
        token: Option<String>,

        #[arg(long)]
        agent_id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum AgentCommand {
    #[command(name = "spawn", about = "Spawn a new agent")]
    Spawn {
        #[arg(long, default_value = "opencode")]
        agent_type: String,

        #[arg(long)]
        cwd: Option<String>,

        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    #[command(name = "list", about = "List running agents")]
    List,

    #[command(name = "kill", about = "Kill an agent")]
    Kill {
        #[arg(long)]
        id: String,
    },
}
