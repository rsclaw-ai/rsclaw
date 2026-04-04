use anyhow::Result;

use crate::config;

/// Detect the active config file and parse it as a JSON value.
/// Note: write-back via `serde_json::to_string_pretty` converts JSON5 to
/// standard JSON (losing comments and trailing commas). Users should use
/// env-var references instead of comments for important annotations.
pub fn load_config_json() -> Result<(std::path::PathBuf, serde_json::Value)> {
    let path = config::loader::detect_config_path()
        .ok_or_else(|| anyhow::anyhow!("no config file found"))?;
    let raw = std::fs::read_to_string(&path)?;
    let val: serde_json::Value = json5::from_str(&raw)?;
    Ok((path, val))
}

pub fn get_nested_value<'a>(
    val: &'a serde_json::Value,
    key: &str,
) -> Option<&'a serde_json::Value> {
    let mut cur = val;
    for part in key.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

pub fn set_nested_value(
    val: &mut serde_json::Value,
    key: &str,
    new_val: serde_json::Value,
) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    let (last, parents) = parts
        .split_last()
        .ok_or_else(|| anyhow::anyhow!("empty key"))?;
    let mut cur = val;
    for part in parents {
        if !cur.is_object() {
            anyhow::bail!("key path `{key}`: intermediate value is not an object");
        }
        cur = cur
            .get_mut(*part)
            .ok_or_else(|| anyhow::anyhow!("missing intermediate key: {part}"))?;
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.insert((*last).to_owned(), new_val);
    } else {
        anyhow::bail!("parent of `{key}` is not an object");
    }
    Ok(())
}

pub fn remove_nested_value(val: &mut serde_json::Value, key: &str) {
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut cur = val;
    for part in parents {
        cur = match cur.get_mut(*part) {
            Some(v) => v,
            None => return,
        };
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.remove(*last);
    }
}
