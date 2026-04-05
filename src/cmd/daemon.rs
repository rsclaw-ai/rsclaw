use anyhow::Result;

use crate::cli::{DaemonCommand, GatewayCommand};
use crate::cmd::gateway::cmd_gateway;

/// Delegate daemon sub-commands to gateway equivalents.
pub async fn cmd_daemon(sub: DaemonCommand) -> Result<()> {
    let gw = match sub {
        DaemonCommand::Install => GatewayCommand::Install,
        DaemonCommand::Start => GatewayCommand::Start,
        DaemonCommand::Stop => GatewayCommand::Stop,
        DaemonCommand::Restart => GatewayCommand::Restart,
        DaemonCommand::Status => GatewayCommand::Status,
        DaemonCommand::Uninstall => GatewayCommand::Uninstall,
    };
    println!("note: `daemon` is a legacy alias for `gateway`");
    cmd_gateway(gw).await
}
