use anyhow::Result;
use std::path::PathBuf;

use super::config_json::{load_config_json, remove_nested_value, set_nested_value};
use super::style::*;
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
            banner(&format!("rsclaw models v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
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
            banner(&format!("rsclaw model scan v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
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
                            item("-", &format!("ollama/{}", m["name"].as_str().unwrap_or("?")));
                        }
                    }
                }
                _ => err_msg("ollama not reachable at localhost:11434"),
            }
        }
        ModelsCommand::Aliases(sub) => match sub {
            AliasesCommand::List => {
                banner(&format!("rsclaw model aliases v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
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
                banner(&format!("rsclaw model fallbacks v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
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
                banner(&format!("rsclaw image fallbacks v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
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
                banner(&format!("rsclaw models auth v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
                println!("  Add a provider to rsclaw.json5:");
                println!();
                println!("  {}", dim(r#"models: {"#));
                println!("  {}", dim(r#"  providers: {"#));
                println!("  {}", dim(r#"    anthropic: { apiKey: "${ANTHROPIC_API_KEY}" },"#));
                println!("  {}", dim(r#"    openai:    { apiKey: "${OPENAI_API_KEY}" },"#));
                println!("  {}", dim(r#"    ollama:    { baseUrl: "http://localhost:11434" },"#));
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
                println!("  {}", dim("Use this token in Authorization: Bearer <token> headers"));
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
                        dim(&format!(
                            "configure via models.providers.{provider}.order"
                        ))
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

struct ModelDef {
    /// CLI name(s) and aliases.
    names: &'static [&'static str],
    /// Display name.
    label: &'static str,
    /// Subdirectory under ~/.rsclaw/models/.
    dir: &'static str,
    /// Download URL (ZIP or tar.bz2).
    url: &'static str,
}

const AVAILABLE_MODELS: &[ModelDef] = &[
    ModelDef {
        names: &["bge", "bge-small-zh"],
        label: "BGE-Small-ZH (Chinese embeddings, ~91MB)",
        dir: "bge-small-zh",
        url: "https://gitfast.org/tools/models/bge-small-zh-v1.5.zip",
    },
    ModelDef {
        names: &["bge-base-zh"],
        label: "BGE-Base-ZH (Chinese embeddings, higher quality, ~400MB)",
        dir: "bge-base-zh",
        url: "https://gitfast.org/tools/models/bge-base-zh-v1.5.zip",
    },
    ModelDef {
        names: &["bge-small-en"],
        label: "BGE-Small-EN (English embeddings, ~127MB)",
        dir: "bge-small-en",
        url: "https://gitfast.org/tools/models/bge-small-en-v1.5.zip",
    },
    ModelDef {
        names: &["whisper", "whisper-tiny"],
        label: "Whisper-Tiny (STT lightweight, ~110MB)",
        dir: "whisper-tiny",
        url: "https://gitfast.org/tools/models/sherpa-onnx-whisper-tiny.tar.bz2",
    },
    ModelDef {
        names: &["whisper-turbo"],
        label: "Whisper-Turbo (STT Chinese recommended, ~537MB)",
        dir: "whisper-turbo",
        url: "https://gitfast.org/tools/models/sherpa-onnx-whisper-turbo.tar.bz2",
    },
    ModelDef {
        names: &["vits", "vits-theresa"],
        label: "VITS-Theresa (Chinese TTS female voice, ~115MB)",
        dir: "vits-theresa",
        url: "https://gitfast.org/tools/models/vits-zh-hf-theresa.tar.bz2",
    },
];

async fn cmd_download_embedding(model: Option<String>) -> Result<()> {
    let model_name = model.as_deref().unwrap_or("bge");
    let base_dir = crate::config::loader::base_dir();

    let def = AVAILABLE_MODELS
        .iter()
        .find(|m| m.names.iter().any(|n| *n == model_name));

    let Some(def) = def else {
        let available: Vec<_> = AVAILABLE_MODELS.iter().map(|m| m.names[0]).collect();
        anyhow::bail!("Unknown model: {model_name}. Available: {}", available.join(", "));
    };

    let model_dir = base_dir.join("models").join(def.dir);
    if model_dir.join("config.json").exists() || model_dir.join("tokens.txt").exists() {
        ok(&format!("{} already installed at {}", def.label, model_dir.display()));
        return Ok(());
    }

    download_archive(def.label, &model_dir, def.url).await
}

fn cmd_list_installed() {
    let base_dir = crate::config::loader::base_dir();
    let models_dir = base_dir.join("models");

    banner(&format!("rsclaw installed models v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));

    let mut found = false;
    for def in AVAILABLE_MODELS {
        let dir = models_dir.join(def.dir);
        // Check for common marker files.
        if dir.join("config.json").exists() || dir.join("tokens.txt").exists() || dir.join("model.onnx").exists() {
            let size = dir_size(&dir);
            println!("  {:<20} {}    {}MB", cyan(def.dir), dim(def.label), size / 1_000_000);
            found = true;
        }
    }

    // Also check bge-base-zh (legacy path).
    let base_zh = models_dir.join("bge-base-zh");
    if base_zh.join("config.json").exists() {
        let size = dir_size(&base_zh);
        println!("  {:<20} {}    {}MB", cyan("bge-base-zh"), dim("(legacy path)"), size / 1_000_000);
        found = true;
    }

    if !found {
        warn_msg("no models installed");
        println!();
        println!("  Run: rsclaw models download");
        println!("  Available: {}", AVAILABLE_MODELS.iter().map(|m| m.names[0]).collect::<Vec<_>>().join(", "));
    }
}

/// Download an archive and extract to dest.
///
/// Reuses the streaming download + extract logic from `cmd/tools.rs`.
async fn download_archive(label: &str, dest: &std::path::Path, url: &str) -> Result<()> {
    println!("Downloading {} ...", bold(label));
    println!("  {} {}", dim("from:"), dim(url));
    std::fs::create_dir_all(dest)?;

    let client = reqwest::Client::new();
    super::tools::download_and_extract_public(&client, url, dest).await?;

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
