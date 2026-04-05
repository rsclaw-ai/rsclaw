use anyhow::Result;

use super::config_json::{
    get_nested_value, load_config_json, remove_nested_value, set_nested_value,
};
use super::style::*;
use crate::{cli::ConfigCommand, config};

pub async fn cmd_config(sub: ConfigCommand) -> Result<()> {
    match sub {
        ConfigCommand::File => match config::loader::detect_config_path() {
            Some(p) => {
                kv("config file", &cyan(&p.display().to_string()));
            }
            None => warn_msg("no config file found"),
        },
        ConfigCommand::Validate => {
            config::load()?;
            ok("config is valid");
        }
        ConfigCommand::Get { key, section: _ } => {
            let (_, val) = load_config_json()?;
            match get_nested_value(&val, &key) {
                Some(v) => {
                    kv(&key, &serde_json::to_string_pretty(v)?);
                }
                None => anyhow::bail!("key not found: {key}"),
            }
        }
        ConfigCommand::Set {
            key,
            value,
            section: _,
        } => {
            let (path, mut val) = load_config_json()?;
            let new_val: serde_json::Value =
                serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
            set_nested_value(&mut val, &key, new_val)?;
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("set {}", cyan(&key)));
        }
        ConfigCommand::Unset { key, section: _ } => {
            let (path, mut val) = load_config_json()?;
            remove_nested_value(&mut val, &key);
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("unset {}", cyan(&key)));
        }
    }
    Ok(())
}
