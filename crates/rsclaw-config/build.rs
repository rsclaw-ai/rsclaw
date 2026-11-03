//! Produce the defaults catalog compiled into the binary
//! (`$OUT_DIR/defaults.toml`, read by `loader::embedded_defaults_toml`).
//!
//! - Default brand, no override: the workspace `defaults.toml`, byte for byte.
//! - Other brands: hosted URLs and data-dir paths follow the brand file.
//! - `RSCLAW_DEFAULTS_OVERRIDE=<file>` (absolute or workspace-relative), or
//!   else the brand directory's `defaults.override.toml`: merged on top; see
//!   `src/defaults_overlay.rs` and docs/branding.md.

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

    let mut shaped = false;
    if rsclaw_brand::ID != "rsclaw" {
        shaped = true;
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
        shaped = true;
    } else if let Some(overlay) = rsclaw_brand::DEFAULTS_OVERRIDE {
        // The brand directory's `defaults.override.toml`.
        text = apply_override(&text, overlay)
            .unwrap_or_else(|e| panic!("brand defaults.override.toml: {e}"));
        shaped = true;
    }

    // A brand or override reshapes the catalog without touching
    // `defaults_version`, so an installed defaults.toml would never pick the
    // change up. Stamp the shaped catalog with a fingerprint of its content;
    // the loader refreshes a file at the same version whose stamp differs.
    if shaped {
        text = stamp_fingerprint(&text);
    }

    if let Err(e) = text.parse::<toml::Table>() {
        panic!("generated defaults catalog is not valid TOML: {e}");
    }
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("defaults.toml");
    fs::write(&out, text).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
}

/// Insert `defaults_fingerprint = "<fnv-1a 64 of the catalog>"` under
/// `[meta]`. FNV-1a, not std's hasher, so the stamp stays the same across
/// toolchains and only moves when the catalog does.
fn stamp_fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let line = format!("defaults_fingerprint = \"{hash:016x}\"\n");
    let mut out = String::with_capacity(text.len() + line.len());
    let mut stamped = false;
    for l in text.split_inclusive('\n') {
        out.push_str(l);
        if !stamped && l.trim_end() == "[meta]" {
            out.push_str(&line);
            stamped = true;
        }
    }
    if !stamped {
        return format!("[meta]\n{line}\n{text}");
    }
    out
}
