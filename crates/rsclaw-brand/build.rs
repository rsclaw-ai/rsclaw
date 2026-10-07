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

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-env-changed=RSCLAW_BRAND_FILE");
    println!("cargo:rerun-if-changed=src/spec.rs");

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
