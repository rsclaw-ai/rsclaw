use std::path::PathBuf;

use anyhow::Result;

use super::{
    config_json::{load_config_json, remove_nested_value, set_nested_value},
    style::*,
};
use crate::{
    cli::{
        AliasesCommand, AuthOrderCommand, FallbacksCommand, ImageFallbacksCommand,
        ModelsAuthCommand, ModelsCommand,
    },
    config,
};

pub async fn cmd_models(sub: ModelsCommand) -> Result<()> {
    match sub {
        ModelsCommand::List | ModelsCommand::Status => {
            banner(&format!("rsclaw models v{}", env!("RSCLAW_BUILD_VERSION")));
            let config = config::load()?;
            if let Some(models_cfg) = &config.model.models {
                println!(
                    "  {:<16} {:<32} {}",
                    bold("PROVIDER"),
                    bold("URL"),
                    bold("STATUS")
                );
                for (name, p) in &models_cfg.providers {
                    let enabled = p.enabled.unwrap_or(true);
                    let url = p.base_url.as_deref().unwrap_or("(default)");
                    let status = if enabled {
                        green("enabled")
                    } else {
                        red("disabled")
                    };
                    println!("  {:<16} {:<32} {}", cyan(name), dim(url), status);
                }
            } else {
                warn_msg("no model providers configured");
            }
            let default_model = config
                .agents
                .defaults
                .model
                .as_ref()
                .and_then(|m| m.primary.as_deref())
                .unwrap_or("anthropic/claude-sonnet-4-5");
            println!();
            kv("default", &bold(default_model));
        }
        ModelsCommand::Set { model } => {
            let (path, mut val) = load_config_json()?;
            set_nested_value(
                &mut val,
                "agents.defaults.model.primary",
                model.clone().into(),
            )?;
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("default model set to '{}'", cyan(&model)));
        }
        ModelsCommand::SetImage { model } => {
            let (path, mut val) = load_config_json()?;
            set_nested_value(
                &mut val,
                "agents.defaults.model.imageModel",
                model.clone().into(),
            )?;
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("image model set to '{}'", cyan(&model)));
        }
        ModelsCommand::Scan => {
            banner(&format!(
                "rsclaw model scan v{}",
                env!("RSCLAW_BUILD_VERSION")
            ));
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?;
            match client.get("http://localhost:11434/api/tags").send().await {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await?;
                    let models = body["models"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                    if models.is_empty() {
                        warn_msg("ollama: no models found");
                    } else {
                        for m in models {
                            item(
                                "-",
                                &format!("ollama/{}", m["name"].as_str().unwrap_or("?")),
                            );
                        }
                    }
                }
                _ => err_msg("ollama not reachable at localhost:11434"),
            }
        }
        ModelsCommand::Aliases(sub) => match sub {
            AliasesCommand::List => {
                banner(&format!(
                    "rsclaw model aliases v{}",
                    env!("RSCLAW_BUILD_VERSION")
                ));
                let config = config::load()?;
                let aliases = config.agents.defaults.models.as_ref();
                if aliases.is_none_or(|a| a.is_empty()) {
                    warn_msg("no model aliases configured");
                } else {
                    for (alias, def) in aliases.unwrap() {
                        let target = def.model.as_deref().or(def.alias.as_deref()).unwrap_or("?");
                        println!("  {} -> {}", cyan(alias), bold(target));
                    }
                }
            }
            AliasesCommand::Add { alias, model } => {
                let (path, mut val) = load_config_json()?;
                set_nested_value(
                    &mut val,
                    &format!("agents.defaults.models.{alias}.model"),
                    model.into(),
                )?;
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok(&format!("alias '{}' added", cyan(&alias)));
            }
            AliasesCommand::Remove { alias } => {
                let (path, mut val) = load_config_json()?;
                remove_nested_value(&mut val, &format!("agents.defaults.models.{alias}"));
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok(&format!("alias '{}' removed", cyan(&alias)));
            }
        },
        ModelsCommand::Fallbacks(sub) => match sub {
            FallbacksCommand::List => {
                banner(&format!(
                    "rsclaw model fallbacks v{}",
                    env!("RSCLAW_BUILD_VERSION")
                ));
                let config = config::load()?;
                let fallbacks = config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.fallbacks.as_deref())
                    .unwrap_or(&[]);
                if fallbacks.is_empty() {
                    warn_msg("no fallback models configured");
                } else {
                    for (i, f) in fallbacks.iter().enumerate() {
                        println!("  {}. {}", dim(&(i + 1).to_string()), cyan(f));
                    }
                }
            }
            FallbacksCommand::Add { model } => {
                let (path, mut val) = load_config_json()?;
                let arr = val
                    .pointer_mut("/agents/defaults/model/fallbacks")
                    .and_then(|v| v.as_array_mut());
                if let Some(arr) = arr {
                    arr.push(model.clone().into());
                } else {
                    set_nested_value(
                        &mut val,
                        "agents.defaults.model.fallbacks",
                        serde_json::json!([model]),
                    )?;
                }
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok(&format!("added fallback '{}'", cyan(&model)));
            }
            FallbacksCommand::Remove { model } => {
                let (path, mut val) = load_config_json()?;
                if let Some(arr) = val
                    .pointer_mut("/agents/defaults/model/fallbacks")
                    .and_then(|v| v.as_array_mut())
                {
                    arr.retain(|v| v.as_str() != Some(&model));
                }
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok(&format!("removed fallback '{}'", cyan(&model)));
            }
            FallbacksCommand::Clear => {
                let (path, mut val) = load_config_json()?;
                set_nested_value(
                    &mut val,
                    "agents.defaults.model.fallbacks",
                    serde_json::json!([]),
                )?;
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok("fallbacks cleared");
            }
        },
        ModelsCommand::ImageFallbacks(sub) => match sub {
            ImageFallbacksCommand::List => {
                banner(&format!(
                    "rsclaw image fallbacks v{}",
                    env!("RSCLAW_BUILD_VERSION")
                ));
                let config = config::load()?;
                let fallbacks = config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.image_fallbacks.as_deref())
                    .unwrap_or(&[]);
                if fallbacks.is_empty() {
                    warn_msg("no image fallback models configured");
                } else {
                    for (i, f) in fallbacks.iter().enumerate() {
                        println!("  {}. {}", dim(&(i + 1).to_string()), cyan(f));
                    }
                }
            }
            ImageFallbacksCommand::Add { model } => {
                let (path, mut val) = load_config_json()?;
                let arr = val
                    .pointer_mut("/agents/defaults/model/imageFallbacks")
                    .and_then(|v| v.as_array_mut());
                if let Some(arr) = arr {
                    arr.push(model.clone().into());
                } else {
                    set_nested_value(
                        &mut val,
                        "agents.defaults.model.imageFallbacks",
                        serde_json::json!([model]),
                    )?;
                }
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok(&format!("added image fallback '{}'", cyan(&model)));
            }
            ImageFallbacksCommand::Remove { model } => {
                let (path, mut val) = load_config_json()?;
                if let Some(arr) = val
                    .pointer_mut("/agents/defaults/model/imageFallbacks")
                    .and_then(|v| v.as_array_mut())
                {
                    arr.retain(|v| v.as_str() != Some(&model));
                }
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok(&format!("removed image fallback '{}'", cyan(&model)));
            }
            ImageFallbacksCommand::Clear => {
                let (path, mut val) = load_config_json()?;
                set_nested_value(
                    &mut val,
                    "agents.defaults.model.imageFallbacks",
                    serde_json::json!([]),
                )?;
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok("image fallbacks cleared");
            }
        },
        ModelsCommand::Auth(sub) => match sub {
            ModelsAuthCommand::Add => {
                banner(&format!(
                    "rsclaw models auth v{}",
                    env!("RSCLAW_BUILD_VERSION")
                ));
                println!("  Add a provider to rsclaw.json5:");
                println!();
                println!("  {}", dim(r#"models: {"#));
                println!("  {}", dim(r#"  providers: {"#));
                println!(
                    "  {}",
                    dim(r#"    anthropic: { apiKey: "${ANTHROPIC_API_KEY}" },"#)
                );
                println!(
                    "  {}",
                    dim(r#"    openai:    { apiKey: "${OPENAI_API_KEY}" },"#)
                );
                println!(
                    "  {}",
                    dim(r#"    ollama:    { baseUrl: "http://localhost:11434" },"#)
                );
                println!("  {}", dim(r#"  }"#));
                println!("  {}", dim(r#"}"#));
            }
            ModelsAuthCommand::SetupToken => {
                let token: String = (0..32)
                    .map(|_| format!("{:02x}", rand::random::<u8>()))
                    .collect();
                let (path, mut val) = load_config_json()?;
                set_nested_value(&mut val, "gateway.authToken", token.clone().into())?;
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok("gateway.authToken generated");
                kv("token", &bold(&token));
                println!(
                    "  {}",
                    dim("Use this token in Authorization: Bearer <token> headers")
                );
            }
            ModelsAuthCommand::PasteToken => {
                use std::io::BufRead as _;
                print!("paste token: ");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let token = std::io::BufReader::new(std::io::stdin())
                    .lines()
                    .next()
                    .transpose()?
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if token.is_empty() {
                    anyhow::bail!("no token provided");
                }
                let (path, mut val) = load_config_json()?;
                set_nested_value(&mut val, "gateway.authToken", token.into())?;
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok("gateway.authToken updated");
            }
            ModelsAuthCommand::Order(sub) => match sub {
                AuthOrderCommand::Get { provider } => {
                    let config = config::load()?;
                    let exists = config
                        .model
                        .models
                        .as_ref()
                        .is_some_and(|m| m.providers.contains_key(&provider));
                    if !exists {
                        anyhow::bail!("provider '{provider}' not found in config");
                    }
                    kv("provider", &cyan(&provider));
                    println!(
                        "  {}",
                        dim(&format!("configure via models.providers.{provider}.order"))
                    );
                }
                AuthOrderCommand::Set { provider, order } => {
                    let (path, mut val) = load_config_json()?;
                    let arr: serde_json::Value =
                        order.into_iter().map(serde_json::Value::String).collect();
                    set_nested_value(&mut val, &format!("models.providers.{provider}.order"), arr)?;
                    std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                    ok(&format!("order set for provider '{}'", cyan(&provider)));
                }
                AuthOrderCommand::Clear { provider } => {
                    let (path, mut val) = load_config_json()?;
                    remove_nested_value(&mut val, &format!("models.providers.{provider}.order"));
                    std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                    ok(&format!("order cleared for provider '{}'", cyan(&provider)));
                }
            },
        },
        ModelsCommand::Download { model } => {
            cmd_download_embedding(model).await?;
        }
        ModelsCommand::Installed => {
            cmd_list_installed();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Embedding model download / list
// ---------------------------------------------------------------------------

const WHISPER_TINY_FILES: &[(&str, &str)] = &[
    (
        "config.json",
        "https://huggingface.co/openai/whisper-tiny/resolve/main/config.json",
    ),
    (
        "tokenizer.json",
        "https://huggingface.co/openai/whisper-tiny/resolve/main/tokenizer.json",
    ),
    (
        "model.safetensors",
        "https://huggingface.co/openai/whisper-tiny/resolve/main/model.safetensors",
    ),
    (
        "generation_config.json",
        "https://huggingface.co/openai/whisper-tiny/resolve/main/generation_config.json",
    ),
];

const BGE_SMALL_ZH_FILES: &[(&str, &str)] = &[
    (
        "config.json",
        "https://huggingface.co/BAAI/bge-small-zh-v1.5/resolve/main/config.json",
    ),
    (
        "tokenizer.json",
        "https://huggingface.co/BAAI/bge-small-zh-v1.5/resolve/main/tokenizer.json",
    ),
    (
        "model.safetensors",
        "https://huggingface.co/BAAI/bge-small-zh-v1.5/resolve/main/model.safetensors",
    ),
];

async fn cmd_download_embedding(model: Option<String>) -> Result<()> {
    let model_name = model.as_deref().unwrap_or("bge");
    let base_dir = crate::config::loader::base_dir();

    match model_name {
        "bge" | "bge-small-zh" => {
            let model_dir = base_dir.join("models/bge-small-zh");
            download_model("BAAI/bge-small-zh-v1.5", &model_dir, BGE_SMALL_ZH_FILES).await?;
        }
        "whisper" | "whisper-tiny" => {
            let model_dir = base_dir.join("models/whisper-tiny");
            download_model("openai/whisper-tiny", &model_dir, WHISPER_TINY_FILES).await?;
        }
        other => {
            anyhow::bail!("Unknown model: {other}. Available: bge, whisper");
        }
    }
    Ok(())
}

fn cmd_list_installed() {
    let base_dir = crate::config::loader::base_dir();
    let models_dir = base_dir.join("models");

    banner(&format!(
        "rsclaw installed models v{}",
        env!("RSCLAW_BUILD_VERSION")
    ));

    let mut found = false;

    // Check bge-small-zh
    let zh_dir = models_dir.join("bge-small-zh");
    if zh_dir.join("config.json").exists() {
        let size = dir_size(&zh_dir);
        println!(
            "  {}    {}    {}MB",
            cyan("bge-small-zh"),
            dim("BAAI/bge-small-zh-v1.5"),
            size / 1_000_000
        );
        found = true;
    }

    // Check bge-small-en
    let en_dir = models_dir.join("bge-small-en");
    if en_dir.join("config.json").exists() {
        let size = dir_size(&en_dir);
        println!(
            "  {}    {}    {}MB",
            cyan("bge-small-en"),
            dim("BAAI/bge-small-en-v1.5"),
            size / 1_000_000
        );
        found = true;
    }

    // Check whisper-tiny
    let whisper_dir = models_dir.join("whisper-tiny");
    if whisper_dir.join("config.json").exists() {
        let size = dir_size(&whisper_dir);
        println!(
            "  {}    {}    {}MB",
            cyan("whisper-tiny"),
            dim("openai/whisper-tiny"),
            size / 1_000_000
        );
        found = true;
    }

    if !found {
        warn_msg("no models installed");
        println!();
        println!("  Run: rsclaw models download");
    }
}

async fn download_model(name: &str, dest: &PathBuf, files: &[(&str, &str)]) -> Result<()> {
    println!("Downloading {} ...", bold(name));
    std::fs::create_dir_all(dest)?;

    let client = reqwest::Client::new();
    for (filename, url) in files {
        let dest_path = dest.join(filename);
        if dest_path.exists() {
            println!("  {} {}", dim(filename), dim("(already exists, skipping)"));
            continue;
        }
        print!("  {} ... ", bold(filename));
        let resp = client.get(*url).send().await?.error_for_status()?;
        let bytes = resp.bytes().await?;
        std::fs::write(&dest_path, &bytes)?;
        println!("{}MB", bytes.len() / 1_000_000);
    }

    println!();
    ok(&format!("model saved to {}", dest.display()));
    Ok(())
}

fn dir_size(path: &PathBuf) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}
