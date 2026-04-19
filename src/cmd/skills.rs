use anyhow::Result;

use super::style::*;
use crate::{cli::SkillsCommand, config, skill};

pub async fn cmd_skills(sub: SkillsCommand) -> Result<()> {
    let config = config::load_quiet()?;
    let global_dir = skill::default_global_skills_dir().unwrap_or_default();
    let registry = skill::load_skills(&global_dir, None, config.ext.skills.as_ref())?;
    let language = config.gateway.language.clone();
    match sub {
        SkillsCommand::List => {
            let mut skills: Vec<_> = registry.all().collect();
            skills.sort_by_key(|s| s.name.as_str());
            if skills.is_empty() {
                println!("No skills installed.");
                println!();
                println!("Install skills with: rsclaw skills install <name>");
                println!("Search skills with:  rsclaw skills search <query>");
            } else {
                println!(
                    "{:<24} {:<8} {}",
                    bold("NAME"),
                    bold("TOOLS"),
                    bold("DESCRIPTION")
                );
                for s in skills {
                    let desc = s.description.as_deref().unwrap_or("-");
                    println!(
                        "{:<24} {:<8} {}",
                        cyan(&s.name),
                        s.tools.len(),
                        desc
                    );
                }
            }
        }
        SkillsCommand::Info { skill } => match registry.get(&skill) {
            Some(s) => {
                println!("{}", bold(&s.name));
                println!();
                println!("  {:<14} {}", dim("Version"), s.version.as_deref().unwrap_or("-"));
                println!("  {:<14} {}", dim("Description"), s.description.as_deref().unwrap_or("-"));
                if !s.tools.is_empty() {
                    println!();
                    println!("  {} ({})", bold("Tools"), s.tools.len());
                    for t in &s.tools {
                        println!("    {} {}", cyan(&t.name), dim(&format!("-- {}", t.description)));
                    }
                }
            }
            None => {
                eprintln!("Skill '{}' not found locally.", skill);
                eprintln!("Use `rsclaw skills search {}` to find it on the registry.", skill);
                std::process::exit(1);
            }
        },
        SkillsCommand::Check { eligible } => {
            let mut skills: Vec<_> = registry.all().collect();
            skills.sort_by_key(|s| s.name.as_str());
            if skills.is_empty() {
                println!("No skills installed.");
                return Ok(());
            }
            for s in skills {
                let runnable = s.tools.iter().all(|t| !t.command.is_empty());
                if eligible && !runnable {
                    continue;
                }
                if runnable {
                    println!("{} {}", green("ok"), s.name);
                } else {
                    println!("{} {} (missing command)", yellow("!!"), s.name);
                }
            }
        }
        SkillsCommand::Install { name } => {
            let client = skill::clawhub::ClawhubClient::new().with_language(language.clone());
            // Check if already installed before printing "Installing".
            let dir_name = name.rsplit_once('@').map(|(_, s)| s).unwrap_or(
                name.rsplit('/').next().unwrap_or(&name)
            );
            let already = skill::clawhub::ClawhubClient::check_installed(&global_dir, dir_name);
            if already {
                print!("Checking '{}'... ", cyan(&name));
            } else {
                print!("Installing '{}'... ", cyan(&name));
            }
            let locked = client.install_with_fallback(&name, &global_dir).await?;
            if already {
                println!("{}", dim(&format!("already up to date (v{})", locked.version)));
            } else {
                println!(
                    "{}",
                    green(&format!("v{} -> {}", locked.version, locked.install_dir.display()))
                );
            }
        }
        SkillsCommand::Uninstall { name } => {
            let skill_dir = global_dir.join(&name);
            if !skill_dir.exists() {
                anyhow::bail!("skill '{name}' not found in {}", global_dir.display());
            }
            std::fs::remove_dir_all(&skill_dir)?;

            let mut lock = skill::clawhub::LockFile::read(&global_dir).unwrap_or_default();
            lock.skills.remove(&name);
            lock.write(&global_dir)?;

            println!("Uninstalled '{}'.", cyan(&name));
        }
        SkillsCommand::Search { query } => {
            let client = skill::clawhub::ClawhubClient::new().with_language(language.clone());
            match client.search_with_fallback(&query).await {
                Ok(results) => {
                    if results.is_empty() {
                        println!("No skills found matching '{}'.", query);
                    } else {
                        let has_stats = results.iter().any(|r| {
                            r.downloads.is_some() || r.installs.is_some() || r.stars.is_some()
                        });
                        if has_stats {
                            println!(
                                "{:<36} {:>10} {:>8}  {:<12}  {}",
                                bold("NAME"),
                                bold("INSTALLS"), bold("STARS"),
                                bold("REGISTRY"),
                                bold("DESCRIPTION"),
                            );
                        } else {
                            println!(
                                "{:<36}  {:<12}  {}",
                                bold("NAME"), bold("REGISTRY"), bold("DESCRIPTION"),
                            );
                        }
                        for r in &results {
                            let desc = r.description.as_deref().unwrap_or("-");
                            let desc: String = if desc.chars().count() > 60 {
                                desc.chars().take(57).collect::<String>() + "..."
                            } else {
                                desc.to_string()
                            };
                            let reg = r.registry.as_str();
                            if has_stats {
                                let inst = r.installs.map(format_count).unwrap_or_else(|| "-".into());
                                let stars = r.stars.map(format_count).unwrap_or_else(|| "-".into());
                                println!(
                                    "{:<36} {:>10} {:>8}  {:<12}  {}",
                                    cyan(&r.slug), inst, stars, dim(reg), desc,
                                );
                            } else {
                                println!(
                                    "{:<36}  {:<12}  {}",
                                    cyan(&r.slug), dim(reg), desc,
                                );
                            }
                        }
                        println!();
                        println!("Install with: rsclaw skills install <name>");
                    }
                }
                Err(e) => {
                    eprintln!("Search failed: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        SkillsCommand::Update { name } => {
            let client = skill::clawhub::ClawhubClient::new().with_language(language.clone());
            let lock = skill::clawhub::LockFile::read(&global_dir).unwrap_or_default();

            let slugs: Vec<String> = if let Some(name) = name {
                vec![name]
            } else {
                lock.skills.keys().cloned().collect()
            };

            if slugs.is_empty() {
                println!("No skills to update.");
                return Ok(());
            }

            for slug in &slugs {
                print!("Updating '{}'... ", cyan(slug));
                match client.install(slug, &global_dir).await {
                    Ok(locked) => println!("{}", green(&format!("v{}", locked.version))),
                    Err(e) => println!("{}", red(&format!("failed: {e:#}"))),
                }
            }
        }
    }
    Ok(())
}

/// Format a count with K/M suffixes for compact display.
fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
