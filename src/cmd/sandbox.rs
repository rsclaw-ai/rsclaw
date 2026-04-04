use anyhow::Result;

use super::style::*;
use crate::{cli::SandboxCommand, config};

pub async fn cmd_sandbox(sub: SandboxCommand) -> Result<()> {
    let config = config::load()?;
    match sub {
        SandboxCommand::List => {
            banner(&format!("rsclaw sandbox v{}", env!("RSCLAW_BUILD_VERSION")));
            match config.ops.sandbox.as_ref() {
                Some(sb) => {
                    let mode = sb
                        .mode
                        .as_ref()
                        .map(|m| format!("{m:?}"))
                        .unwrap_or_else(|| "off".to_owned());
                    kv("mode", &bold(&mode));
                    if let Some(docker) = &sb.docker {
                        kv(
                            "docker.image",
                            docker.image.as_deref().unwrap_or("(default)"),
                        );
                        kv(
                            "docker.network",
                            docker.network.as_deref().unwrap_or("none"),
                        );
                    } else {
                        kv("docker", &dim("not configured"));
                    }
                }
                None => warn_msg("sandbox: not configured"),
            }
        }
        SandboxCommand::Explain => {
            banner(&format!(
                "rsclaw sandbox explain v{}",
                env!("RSCLAW_BUILD_VERSION")
            ));
            println!("  Each agent runs in its own isolated environment.");
            println!(
                "  Configure via {} in rsclaw.json5 / openclaw.json",
                cyan("sandbox.docker.image")
            );
        }
        SandboxCommand::Recreate { id } => {
            // Container recreation requires the gateway runtime; just validate config here.
            match config.ops.sandbox.as_ref() {
                None => anyhow::bail!("sandbox is not configured"),
                Some(sb) => {
                    let image = sb
                        .docker
                        .as_ref()
                        .and_then(|d| d.image.as_deref())
                        .unwrap_or("(default)");
                    let target = id.as_deref().unwrap_or("all");
                    ok(&format!(
                        "sandbox recreate '{}' -- image:{}",
                        cyan(target),
                        bold(image)
                    ));
                    println!(
                        "  {}",
                        dim(
                            "Send SIGUSR1 to the gateway process or restart it to rebuild containers"
                        )
                    );
                }
            }
        }
    }
    Ok(())
}
