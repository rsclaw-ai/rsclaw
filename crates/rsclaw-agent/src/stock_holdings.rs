//! Persistent A-share holdings and watchlist management.

use std::{
    fs::{File, OpenOptions},
    io::{BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
struct HoldingEntry {
    code: String,
    name: String,
    cost: f64,
    shares: u64,
}

fn normalize_code(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let (code, suffix) = trimmed
        .split_once('.')
        .map_or((trimmed, None), |(code, suffix)| (code, Some(suffix)));
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("stock code must contain exactly six digits");
    }
    if suffix.is_some_and(|suffix| !matches!(suffix.to_ascii_uppercase().as_str(), "SH" | "SZ")) {
        bail!("stock code suffix must be .SH or .SZ");
    }
    Ok(code.to_owned())
}

fn configured_path() -> PathBuf {
    if let Some(path) = std::env::var_os("RSCLAW_HOLDINGS_PATH").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }

    let config_path = rsclaw_config::loader::base_dir().join("rsclaw.json5");
    if let Ok(raw) = std::fs::read_to_string(config_path) {
        if let Ok(config) = json5::from_str::<Value>(&raw) {
            if let Some(path) = config
                .get("astock")
                .and_then(|astock| {
                    astock
                        .get("holdingsPath")
                        .or_else(|| astock.get("holdings_path"))
                })
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty())
            {
                return PathBuf::from(path);
            }
        }
    }

    #[cfg(windows)]
    {
        let legacy = PathBuf::from(r"K:\openclaw\workspace-multi-agent\holdings_config.json");
        if legacy.exists() {
            return legacy;
        }
    }

    rsclaw_config::loader::base_dir().join("holdings_config.json")
}

fn read_entries(path: &Path) -> Result<Vec<HoldingEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path)
        .with_context(|| format!("failed to open holdings file `{}`", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("invalid holdings JSON in `{}`", path.display()))
}

fn write_entries(path: &Path, entries: &[HoldingEntry]) -> Result<()> {
    let parent = path
        .parent()
        .context("holdings path must have a parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create holdings directory `{}`", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "failed to create temporary holdings file in `{}`",
            parent.display()
        )
    })?;
    serde_json::to_writer_pretty(&mut temporary, entries)
        .context("failed to serialize holdings")?;
    temporary
        .write_all(b"\n")
        .context("failed to finish holdings JSON")?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("failed to flush holdings JSON")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace holdings file `{}`", path.display()))?;
    Ok(())
}

fn shares_arg(args: &Value) -> Result<Option<u64>> {
    args.get("shares")
        .map(|value| {
            value
                .as_u64()
                .context("stock_holdings: `shares` must be a non-negative integer")
        })
        .transpose()
}

fn mutate(path: &Path, args: &Value) -> Result<Value> {
    let action = args["action"]
        .as_str()
        .context("stock_holdings: `action` required")?;
    if action == "list" {
        let entries = read_entries(path)?;
        return Ok(json!({
            "success": true,
            "count": entries.len(),
            "holdings": entries,
        }));
    }

    let parent = path
        .parent()
        .context("holdings path must have a parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create holdings directory `{}`", parent.display()))?;
    let lock_path = parent.join(format!(
        ".{}.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("holdings_config.json")
    ));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open holdings lock `{}`", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("failed to lock holdings file `{}`", path.display()))?;

    let result = (|| {
        let mut entries = read_entries(path)?;
        let code = normalize_code(
            args["code"]
                .as_str()
                .context("stock_holdings: `code` required for this action")?,
        )?;

        match action {
            "add" => {
                if entries.iter().any(|entry| entry.code == code) {
                    bail!("stock `{code}` is already in holdings");
                }
                let name = args["name"]
                    .as_str()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .context("stock_holdings: non-empty `name` required for add")?;
                let cost = args.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
                if !cost.is_finite() || cost < 0.0 {
                    bail!("stock_holdings: `cost` must be a finite non-negative number");
                }
                entries.push(HoldingEntry {
                    code: code.clone(),
                    name: name.to_owned(),
                    cost,
                    shares: shares_arg(args)?.unwrap_or(0),
                });
            }
            "update" => {
                let entry = entries
                    .iter_mut()
                    .find(|entry| entry.code == code)
                    .with_context(|| format!("stock `{code}` is not in holdings"))?;
                let mut changed = false;
                if let Some(name) = args.get("name").and_then(Value::as_str) {
                    let name = name.trim();
                    if name.is_empty() {
                        bail!("stock_holdings: `name` cannot be empty");
                    }
                    entry.name = name.to_owned();
                    changed = true;
                }
                if let Some(cost) = args.get("cost").and_then(Value::as_f64) {
                    if !cost.is_finite() || cost < 0.0 {
                        bail!("stock_holdings: `cost` must be a finite non-negative number");
                    }
                    entry.cost = cost;
                    changed = true;
                }
                if let Some(shares) = shares_arg(args)? {
                    entry.shares = shares;
                    changed = true;
                }
                if !changed {
                    bail!("stock_holdings: update requires `name`, `cost`, or `shares`");
                }
            }
            "remove" => {
                let before = entries.len();
                entries.retain(|entry| entry.code != code);
                if entries.len() == before {
                    bail!("stock `{code}` is not in holdings");
                }
            }
            _ => bail!("stock_holdings: unknown action `{action}`"),
        }

        write_entries(path, &entries)?;
        Ok(json!({
            "success": true,
            "action": action,
            "code": code,
            "count": entries.len(),
            "holdings": entries,
        }))
    })();

    FileExt::unlock(&lock).context("failed to unlock holdings file")?;
    result
}

/// Handle one structured holdings/watchlist operation for the agent.
pub async fn handle(args: &Value) -> Result<Value> {
    let args = args.clone();
    tokio::task::spawn_blocking(move || mutate(&configured_path(), &args))
        .await
        .context("stock holdings task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_update_remove_preserves_existing_shape() {
        let directory = tempfile::tempdir().expect("create temporary holdings directory");
        let path = directory.path().join("holdings_config.json");

        mutate(
            &path,
            &json!({"action": "add", "code": "002594.SZ", "name": "比亚迪"}),
        )
        .expect("add watchlist stock");
        mutate(
            &path,
            &json!({"action": "update", "code": "002594", "cost": 188.5, "shares": 100}),
        )
        .expect("update holding");

        assert_eq!(
            read_entries(&path).expect("read updated holdings"),
            vec![HoldingEntry {
                code: "002594".to_owned(),
                name: "比亚迪".to_owned(),
                cost: 188.5,
                shares: 100,
            }]
        );

        mutate(&path, &json!({"action": "remove", "code": "002594"})).expect("remove holding");
        assert!(read_entries(&path).expect("read empty holdings").is_empty());
    }

    #[test]
    fn malformed_json_is_not_overwritten() {
        let directory = tempfile::tempdir().expect("create temporary holdings directory");
        let path = directory.path().join("holdings_config.json");
        std::fs::write(&path, "not valid json\n").expect("write malformed fixture");

        let error = mutate(
            &path,
            &json!({"action": "add", "code": "600519", "name": "贵州茅台"}),
        )
        .expect_err("malformed JSON must reject mutation");
        assert!(error.to_string().contains("invalid holdings JSON"));
        assert_eq!(
            std::fs::read_to_string(path).expect("read fixture after rejection"),
            "not valid json\n"
        );
    }
}
