//! Generates `$OUT_DIR/brand.rs` from the selected brand file.
//!
//! `RSCLAW_BRAND_FILE` selects the file (absolute, or relative to the
//! workspace root); otherwise `brand/rsclaw.toml` is used.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[allow(dead_code)]
mod spec {
    include!("src/spec.rs");
}

#[allow(dead_code)]
mod plugin_digest {
    include!("src/plugin_digest.rs");
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-env-changed=RSCLAW_BRAND_FILE");
    println!("cargo:rerun-if-changed=src/spec.rs");
    println!("cargo:rerun-if-changed=src/plugin_digest.rs");

    let path = match env::var("RSCLAW_BRAND_FILE") {
        Ok(p) if !p.trim().is_empty() => {
            let p = PathBuf::from(p.trim());
            if p.is_absolute() {
                p
            } else {
                manifest_dir.join("../..").join(p)
            }
        }
        _ => manifest_dir.join("brand/rsclaw.toml"),
    };
    println!("cargo:rerun-if-changed={}", path.display());

    let src = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read brand file {}: {e}", path.display()));
    let spec = spec::parse(&src)
        .and_then(|s| spec::validate(&s).map(|()| s))
        .unwrap_or_else(|e| panic!("invalid brand file {}: {e}", path.display()));

    // Optional assets next to the brand file (see docs/branding.md, "Brand
    // directory"). Watching the directory picks up added or removed files.
    let brand_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    println!("cargo:rerun-if-changed={}", brand_dir.display());
    let found = |rel: &str| -> Option<String> {
        let file = brand_dir.join(rel);
        if !file.is_file() {
            return None;
        }
        println!("cargo:rerun-if-changed={}", file.display());
        check_asset(&file).unwrap_or_else(|e| panic!("invalid brand asset {}: {e}", file.display()));
        Some(file.to_string_lossy().into_owned())
    };
    let mut code = spec::generate(&spec);
    code.push_str(&spec::generate_assets(&found));
    let plugins = bundled_plugins(&brand_dir.join("plugins"))
        .unwrap_or_else(|e| panic!("invalid bundled plugin under {}: {e}", brand_dir.join("plugins").display()));
    code.push_str(&spec::generate_bundled_plugins(&plugins));

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("brand.rs");
    fs::write(&out, code).unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
}

/// Build-time validation of one asset, by extension.
fn check_asset(file: &Path) -> Result<(), String> {
    let src = fs::read_to_string(file).map_err(|e| format!("not readable as UTF-8: {e}"))?;
    if src.trim().is_empty() {
        return Err("file is empty".to_owned());
    }
    match file.extension().and_then(|e| e.to_str()) {
        Some("json5") => match json5::from_str::<serde_json::Value>(&src) {
            Ok(serde_json::Value::Object(_)) => Ok(()),
            Ok(_) => Err("top level must be an object".to_owned()),
            Err(e) => Err(format!("not valid JSON5: {e}")),
        },
        Some("toml") => src
            .parse::<toml::Table>()
            .map(|_| ())
            .map_err(|e| format!("not valid TOML: {e}")),
        _ => Ok(()),
    }
}

/// Discover `plugins/<name>/` next to the brand file: each needs a
/// `plugin.json5` whose `name` equals the directory and a `version`. Every
/// file is watched so a rebuilt plugin refreshes the recorded digest.
fn bundled_plugins(dir: &Path) -> Result<Vec<spec::BundledPluginSpec>, String> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read: {e}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    let mut out = Vec::new();
    for plugin_dir in entries {
        let dir_name = plugin_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        let valid_slug = !dir_name.is_empty()
            && dir_name.len() <= 64
            && !dir_name.starts_with(['-', '.'])
            && dir_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid_slug {
            return Err(format!("`{dir_name}` is not a valid plugin directory name"));
        }
        watch_tree(&plugin_dir);
        let manifest_path = plugin_dir.join("plugin.json5");
        let src = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("{dir_name}: cannot read plugin.json5: {e}"))?;
        let manifest: serde_json::Value =
            json5::from_str(&src).map_err(|e| format!("{dir_name}: plugin.json5 is not valid JSON5: {e}"))?;
        let name = manifest.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        if name != dir_name {
            return Err(format!("{dir_name}: plugin.json5 name `{name}` must equal the directory name"));
        }
        let version = manifest
            .get("version")
            .and_then(|v| v.as_str())
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| format!("{dir_name}: plugin.json5 needs a version"))?;
        if let Some(entry) = manifest.get("entry").and_then(|v| v.as_str()) {
            let entry_path = plugin_dir.join(entry.trim_start_matches("./"));
            if !entry_path.is_file() {
                return Err(format!("{dir_name}: entry `{entry}` is missing"));
            }
        }
        let digest = plugin_digest::tree_digest(&plugin_dir)
            .map_err(|e| format!("{dir_name}: cannot hash: {e}"))?;
        out.push((dir_name, version.to_owned(), digest));
    }
    Ok(out)
}

fn watch_tree(dir: &Path) {
    println!("cargo:rerun-if-changed={}", dir.display());
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                watch_tree(&path);
            } else {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

