//! Produce the defaults catalog compiled into the binary
//! (`$OUT_DIR/defaults.toml`, read by `loader::embedded_defaults_toml`).
//!
//! - Default brand, no override: the workspace `defaults.toml`, byte for byte.
//! - Other brands: hosted URLs and data-dir paths follow the brand file.
//! - `RSCLAW_DEFAULTS_OVERRIDE=<file>` (absolute or workspace-relative):
//!   merged on top; see `src/defaults_overlay.rs` and docs/branding.md.

use std::{env, fs, path::PathBuf};

include!("src/defaults_overlay.rs");

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../..");
    let base = root.join("defaults.toml");
    println!("cargo:rerun-if-changed={}", base.display());
    println!("cargo:rerun-if-env-changed=RSCLAW_DEFAULTS_OVERRIDE");

    let mut text = fs::read_to_string(&base)
        .unwrap_or_else(|e| panic!("read {}: {e}", base.display()));

    if rsclaw_brand::ID != "rsclaw" {
        text = rebrand_catalog(
            &text,
            &CatalogBrand {
                api_url: rsclaw_brand::API_URL,
                hub_url: rsclaw_brand::HUB_URL,
                app_url: rsclaw_brand::APP_URL,
                home_dir: rsclaw_brand::HOME_DIR,
                config_file: rsclaw_brand::CONFIG_FILE,
            },
        );
    }

    if let Some(raw) = env::var_os("RSCLAW_DEFAULTS_OVERRIDE").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(raw);
        let path = if path.is_absolute() { path } else { root.join(path) };
        println!("cargo:rerun-if-changed={}", path.display());
        let overlay = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("RSCLAW_DEFAULTS_OVERRIDE {}: {e}", path.display()));
        text = apply_override(&text, &overlay)
            .unwrap_or_else(|e| panic!("RSCLAW_DEFAULTS_OVERRIDE {}: {e}", path.display()));
    }

    if let Err(e) = text.parse::<toml::Table>() {
        panic!("generated defaults catalog is not valid TOML: {e}");
    }
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("defaults.toml");
    fs::write(&out, text).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
}
