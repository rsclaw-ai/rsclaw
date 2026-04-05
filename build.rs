fn main() {
    // Provide a fallback for RSCLAW_BUILD_VERSION so the crate compiles
    // even when the env var is not set (e.g. when built as a dependency
    // from crates.io).
    if std::env::var("RSCLAW_BUILD_VERSION").is_err() {
        let version = env!("CARGO_PKG_VERSION");
        println!("cargo:rustc-env=RSCLAW_BUILD_VERSION={version}");
    }

    if std::env::var("RSCLAW_BUILD_DATE").is_err() {
        println!("cargo:rustc-env=RSCLAW_BUILD_DATE=unknown");
    }
}
