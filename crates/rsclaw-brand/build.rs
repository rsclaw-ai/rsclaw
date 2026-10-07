//! Generates `$OUT_DIR/brand.rs` from the selected brand file.
//!
//! `RSCLAW_BRAND_FILE` selects the file (absolute, or relative to the
//! workspace root); otherwise `brand/rsclaw.toml` is used.

use std::{env, fs, path::PathBuf};

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

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("brand.rs");
    fs::write(&out, spec::generate(&spec))
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
}
