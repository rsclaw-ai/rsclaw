use anyhow::Result;

use crate::cli::anycli::AnycliCommand;

/// Handle `rsclaw anycli` subcommands.
pub async fn cmd_anycli(sub: AnycliCommand) -> Result<()> {
    match sub {
        AnycliCommand::List => {
            let registry = anycli::Registry::load()?;
            println!("{:<20} {}", "ADAPTER", "DESCRIPTION");
            println!("{:<20} {}", "-------", "-----------");
            for adapter in registry.list() {
                println!("{:<20} {}", adapter.name, adapter.description);
            }
        }

        AnycliCommand::Info { adapter: name } => {
            let registry = anycli::Registry::load()?;
            let adapter = registry.find(&name)?;
            println!("Name:        {}", adapter.name);
            println!("Description: {}", adapter.description);
            println!("Base URL:    {}", adapter.base_url);
            if !adapter.version.is_empty() {
                println!("Version:     {}", adapter.version);
            }
            println!("\nCommands:");
            for (cmd_name, cmd) in &adapter.commands {
                println!("  {cmd_name:<16} {}", cmd.description);
                for (param_name, param) in &cmd.params {
                    let req = if param.required { " (required)" } else { "" };
                    let desc = param.description.as_deref().unwrap_or("");
                    let default = param
                        .default
                        .as_ref()
                        .map(|d| format!(" [default: {d}]"))
                        .unwrap_or_default();
                    println!("    {param_name:<14} {desc}{default}{req}");
                }
            }
        }

        AnycliCommand::Run {
            adapter: name,
            command,
            params,
            format,
        } => {
            let registry = anycli::Registry::load()?;
            let adapter = registry.find(&name)?;
            let fmt: anycli::OutputFormat = format.parse()?;

            let parsed: Vec<(String, String)> = params
                .iter()
                .filter_map(|p| {
                    let (k, v) = p.split_once('=')?;
                    Some((k.to_owned(), v.to_owned()))
                })
                .collect();

            let param_refs: Vec<(&str, &str)> = parsed
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();

            let result = anycli::Pipeline::execute(adapter, &command, &param_refs).await?;
            println!("{}", result.format(fmt)?);
        }

        AnycliCommand::Search { query } => {
            let hub = anycli::Hub::new()?;
            let results = hub.search(&query).await?;
            if results.is_empty() {
                println!("No adapters found for `{query}`");
            } else {
                println!("{:<20} {}", "ADAPTER", "DESCRIPTION");
                println!("{:<20} {}", "-------", "-----------");
                for entry in &results {
                    println!("{:<20} {}", entry.name, entry.description);
                }
                println!("\nInstall: rsclaw anycli install <name>");
            }
        }

        AnycliCommand::Install { name } => {
            let hub = anycli::Hub::new()?;
            let dir = anycli::hub::default_adapters_dir()
                .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
            let path = hub.install(&name, &dir).await?;
            println!("Installed `{name}` to {}", path.display());
        }

        AnycliCommand::Update => {
            let hub = anycli::Hub::new()?;
            let dir = anycli::hub::default_adapters_dir()
                .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
            let (updated, total) = hub.update(&dir).await?;
            println!("Updated {updated}/{total} adapters");
        }
    }

    Ok(())
}
