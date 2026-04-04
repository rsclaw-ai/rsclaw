use anyhow::Result;

use super::style::*;
use crate::{cli::SecretsCommand, config};

pub async fn cmd_secrets(sub: SecretsCommand) -> Result<()> {
    let config = config::load()?;
    match sub {
        SecretsCommand::Reload => {
            ok(
                "secrets reloaded -- restart the gateway to apply (hot reload not supported for secrets)",
            );
        }
        SecretsCommand::Audit => {
            banner(&format!(
                "rsclaw secrets audit v{}",
                env!("RSCLAW_BUILD_VERSION")
            ));
            let secrets = config.ops.secrets.as_ref();
            if let Some(s) = secrets {
                println!("  {:<20} {}", bold("PROVIDER"), bold("TYPE"));
                for (name, p) in &s.providers {
                    println!("  {:<20} {}", cyan(name), dim(&format!("{:?}", p.kind)));
                }
            } else {
                warn_msg("no secrets configured");
            }
        }
        SecretsCommand::Configure => {
            banner(&format!(
                "rsclaw secrets configure v{}",
                env!("RSCLAW_BUILD_VERSION")
            ));
            println!("  Add a secrets provider to rsclaw.json5:");
            println!();
            println!("  {}", dim(r#"secrets: {"#));
            println!("  {}", dim(r#"  providers: {"#));
            println!(
                "  {}",
                dim(r#"    // env: reads from environment variables"#)
            );
            println!("  {}", dim(r#"    env: { type: "env" },"#));
            println!("  {}", dim(r#"    // file: reads from a JSON/dotenv file"#));
            println!(
                "  {}",
                dim(r#"    file: { type: "file", file: "~/.rsclaw/secrets.json" },"#)
            );
            println!("  {}", dim(r#"  }"#));
            println!("  {}", dim(r#"}"#));
            println!();
            println!(
                "  Then reference secrets in config as: {}",
                cyan("${MY_SECRET_KEY}")
            );
        }
        SecretsCommand::Apply(args) => {
            let from_path = std::path::Path::new(&args.from);
            if !from_path.exists() {
                anyhow::bail!("secrets file not found: {}", args.from);
            }
            let raw = std::fs::read_to_string(from_path)?;
            // Parse as either JSON or .env (key=value lines)
            let mut count = 0usize;
            if args.from.ends_with(".json") || args.from.ends_with(".json5") {
                let val: serde_json::Value = json5::from_str(&raw)?;
                if let Some(obj) = val.as_object() {
                    for (k, v) in obj {
                        let s = v.as_str().unwrap_or_default();
                        if !args.dry_run {
                            // SAFETY: secrets apply is intentionally setting env vars
                            unsafe { std::env::set_var(k, s) };
                        }
                        let label = if args.dry_run {
                            yellow("[dry-run]")
                        } else {
                            green("set    ")
                        };
                        println!(
                            "  {}  {}={}",
                            label,
                            cyan(k),
                            &s[..s.len().min(4)].replace(|_: char| true, "*")
                        );
                        count += 1;
                    }
                }
            } else {
                // .env format: KEY=value lines
                for line in raw.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = line.split_once('=') {
                        if !args.dry_run {
                            unsafe { std::env::set_var(k.trim(), v.trim()) };
                        }
                        let label = if args.dry_run {
                            yellow("[dry-run]")
                        } else {
                            green("set    ")
                        };
                        println!("  {}  {}=****", label, cyan(k.trim()));
                        count += 1;
                    }
                }
            }
            if args.dry_run {
                warn_msg(&format!(
                    "{} secret(s) would be applied",
                    bold(&count.to_string())
                ));
            } else {
                ok(&format!(
                    "{} secret(s) applied to environment",
                    bold(&count.to_string())
                ));
            }
        }
    }
    Ok(())
}
