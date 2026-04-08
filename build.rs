fn main() {
    // Set RSCLAW_BUILD_VERSION and RSCLAW_BUILD_DATE at compile time.
    // CI overrides these via env vars; local dev gets sensible defaults.
    if std::env::var("RSCLAW_BUILD_VERSION").is_err() {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "dev".to_owned());
        println!("cargo:rustc-env=RSCLAW_BUILD_VERSION=v{version}");
    }
    if std::env::var("RSCLAW_BUILD_DATE").is_err() {
        // Simple date without external crates
        let output = std::process::Command::new("date").arg("+%Y-%m-%d").output();
        let date = output
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|| "unknown".to_owned());
        println!("cargo:rustc-env=RSCLAW_BUILD_DATE={date}");
    }
}
