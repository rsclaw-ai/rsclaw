use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DnsCommand {
    /// Generate CoreDNS config for Tailscale wide-area Bonjour discovery.
    Setup {
        #[arg(long)]
        domain: Option<String>,
    },
}
