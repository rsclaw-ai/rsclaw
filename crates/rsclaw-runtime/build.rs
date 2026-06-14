fn main() {
    // Set RSCLAW_BUILD_VERSION and RSCLAW_BUILD_DATE at compile time.
    // CI overrides these via env vars; local dev gets sensible defaults.
    //
    // VERSION CONVENTION:
    //   RSCLAW_BUILD_VERSION stores the bare version WITHOUT "v" prefix.
    //   Display code (preparse.rs, main.rs) adds "v" when showing to users.
    //   CI/scripts should pass bare versions: RSCLAW_BUILD_VERSION=2026.4.15
    //   NOT: RSCLAW_BUILD_VERSION=v2026.4.15
    let version = std::env::var("RSCLAW_BUILD_VERSION").unwrap_or_else(|_| {
        std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "dev".to_owned())
    });
    // Defensive: strip a leading 'v' so CI mistakes don't produce vv2026.5.1.
    let version = version.trim_start_matches('v');
    println!("cargo:rustc-env=RSCLAW_BUILD_VERSION={version}");

    // Short commit hash (6 chars). CI can override; locally we shell out
    // to git. `cargo:rerun-if-changed=.git/HEAD` makes cargo rebuild when
    // HEAD moves so `rsclaw -v` reflects the current commit, not a stale
    // one cached from the previous build.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    if std::env::var("RSCLAW_BUILD_COMMIT").is_err() {
        let commit = std::process::Command::new("git")
            .args(["rev-parse", "--short=6", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        println!("cargo:rustc-env=RSCLAW_BUILD_COMMIT={commit}");
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
