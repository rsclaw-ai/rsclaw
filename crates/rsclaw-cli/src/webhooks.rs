use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum WebhooksCommand {
    /// Set up Gmail Pub/Sub webhook.
    Gmail,
}
