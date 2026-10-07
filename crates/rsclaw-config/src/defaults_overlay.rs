// Build-time shaping of the defaults catalog compiled into the binary.
//
// Shared by `build.rs` (via `include!`) and the crate's unit tests, so it
// must not refer to `crate::` items and may only depend on `toml_edit` + `std`.

/// Identity values a non-default brand substitutes into the catalog.
pub struct CatalogBrand<'a> {
    pub api_url: &'a str,
    pub hub_url: &'a str,
    pub app_url: &'a str,
    pub home_dir: &'a str,
    pub config_file: &'a str,
}

/// Rewrite the default brand's hosted URLs and data-dir paths (including
/// the regex-escaped forms used by `exec_safety`) to `brand`'s values.
/// Text-level, so comments and layout survive.
pub fn rebrand_catalog(text: &str, brand: &CatalogBrand<'_>) -> String {
    // In the TOML source a regex `\.` is written `\\.`.
    let toml_regex = |s: &str| s.replace('.', "\\\\.");
    text.replace("https://api.rsclaw.ai", brand.api_url)
        .replace("https://hub.rsclaw.ai", brand.hub_url)
        .replace("https://app.rsclaw.ai", brand.app_url)
        .replace("\\\\.rsclaw/", &format!("{}/", toml_regex(brand.home_dir)))
        .replace("rsclaw\\\\.json5", &toml_regex(brand.config_file))
}

/// Merge an override document into the catalog, preserving the catalog's
/// formatting.
///
/// - Tables merge key by key, recursively.
/// - Arrays of tables (`[[providers]]`, `[[channels]]`, ...) match entries
///   by `name`: a matching entry is merged, `_remove = true` drops it, and
///   an unmatched entry is appended.
/// - Any other value replaces the catalog's value.
pub fn apply_override(catalog: &str, overlay: &str) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = catalog
        .parse()
        .map_err(|e| format!("defaults catalog is not valid TOML: {e}"))?;
    let over: toml_edit::DocumentMut = overlay
        .parse()
        .map_err(|e| format!("override is not valid TOML: {e}"))?;
    merge_table(doc.as_table_mut(), over.as_table(), "")?;
    Ok(doc.to_string())
}

fn merge_table(base: &mut toml_edit::Table, over: &toml_edit::Table, path: &str) -> Result<(), String> {
    for (key, item) in over.iter() {
        if key == "_remove" {
            continue;
        }
        let here = if path.is_empty() { key.to_owned() } else { format!("{path}.{key}") };
        match (base.get_mut(key), item) {
            (Some(toml_edit::Item::Table(b)), toml_edit::Item::Table(o)) => merge_table(b, o, &here)?,
            (Some(toml_edit::Item::ArrayOfTables(b)), toml_edit::Item::ArrayOfTables(o)) => {
                merge_named(b, o, &here)?;
            }
            _ => {
                let mut item = item.clone();
                strip_remove_markers(&mut item);
                base.insert(key, item);
            }
        }
    }
    Ok(())
}

fn merge_named(
    base: &mut toml_edit::ArrayOfTables,
    over: &toml_edit::ArrayOfTables,
    path: &str,
) -> Result<(), String> {
    for entry in over.iter() {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("every `[[{path}]]` override entry needs a string `name`"))?
            .to_owned();
        let remove = entry.get("_remove").and_then(|v| v.as_bool()).unwrap_or(false);
        let pos = base
            .iter()
            .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name.as_str()));
        match (pos, remove) {
            (Some(i), true) => base.remove(i),
            (None, true) => return Err(format!("`[[{path}]]` `{name}`: nothing to remove")),
            (Some(i), false) => {
                let target = base.get_mut(i).expect("index from position");
                merge_table(target, entry, &format!("{path}.{name}"))?;
            }
            (None, false) => {
                let mut t = entry.clone();
                t.remove("_remove");
                base.push(t);
            }
        }
    }
    Ok(())
}

fn strip_remove_markers(item: &mut toml_edit::Item) {
    if let toml_edit::Item::ArrayOfTables(arr) = item {
        for t in arr.iter_mut() {
            t.remove("_remove");
        }
    }
}
