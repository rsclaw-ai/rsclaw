use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum SystemCommand {
    Event,
    #[command(subcommand)]
    Heartbeat(HeartbeatCommand),
    Presence,
}

#[derive(Subcommand, Debug)]
pub enum HeartbeatCommand {
    Last,
    Enable,
    Disable,
}
