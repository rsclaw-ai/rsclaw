use anyhow::Result;

use crate::cli::dns::DnsCommand;

pub async fn cmd_dns(sub: DnsCommand) -> Result<()> {
    match sub {
        DnsCommand::Setup { domain } => {
            let domain = domain.as_deref().unwrap_or("ts.net");
            println!("# CoreDNS config for Tailscale wide-area Bonjour discovery");
            println!("# Domain: {domain}");
            println!("#");
            println!("# Add this to your CoreDNS Corefile (typically /etc/coredns/Corefile)");
            println!("# or pass via --conf flag to coredns.");
            println!();
            println!("_openclaw._tcp.{domain} {{");
            println!("    hosts {{");
            println!("        # Add entries for each node:");
            println!("        # <tailscale-ip> <hostname>.{domain}");
            println!("        fallthrough");
            println!("    }}");
            println!("    mdns {{");
            println!("        browse _openclaw._tcp.local.");
            println!("    }}");
            println!("}}");
            println!();
            println!("# Instructions:");
            println!("# 1. Install CoreDNS: brew install coredns (macOS) or apt install coredns");
            println!("# 2. Copy this config to your Corefile");
            println!("# 3. Add your Tailscale node IPs and hostnames");
            println!("# 4. Run: coredns -conf /path/to/Corefile");
            println!("# 5. Configure Tailscale DNS to use this CoreDNS instance");
        }
    }
    Ok(())
}
