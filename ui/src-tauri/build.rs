use std::{path::PathBuf, process::Command};

const APP_COMMANDS: &[&str] = &[
    "stream_fetch",
    "start_gateway",
    "stop_gateway",
    "gateway_status",
    "get_config_path",
    "clear_webview_cache_dirs",
    "run_setup",
    "write_config",
    "read_config_file",
    "get_channel_accounts",
    "check_setup",
    "get_gateway_port",
    "get_version",
    "scan_openclaw",
    "detect_openclaw",
    "channel_login_start",
    "channel_login_qr",
    "channel_login_status",
    "get_cron_jobs",
    "save_cron_jobs",
    "get_skills",
    "install_skill",
    "uninstall_skill",
    "search_skills",
    "get_plugins",
    "install_plugin",
    "uninstall_plugin",
    "install_tool",
    "get_tool_install_status",
    "get_tool_install_result",
    "read_defaults_catalog",
    "test_provider",
    "write_workspace_file",
    "read_workspace_file",
    "run_rsclaw_cli",
    "migrate_openclaw",
    "set_auto_start",
    "get_auto_start",
    "set_gateway_user_stopped",
    "get_gateway_user_stopped",
    "open_path",
    "save_attach_image",
    "read_file_as_data_url",
    "open_glow_overlay",
    "close_glow_overlay",
    "open_rsclaw_console",
    "close_rsclaw_console",
    "rsclaw_console_install_key",
];

fn main() {
    check_sidecar_version();
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(APP_COMMANDS));
    tauri_build::try_build(attributes).expect("failed to build Tauri application metadata")
}

/// Fail the desktop build if the bundled rsclaw sidecar doesn't match
/// the desktop crate's version.
///
/// Why: the desktop's tools/skills/plugins install commands shell out
/// to the bundled sidecar. When the sidecar drifts behind the workspace
/// — e.g. someone bumps the workspace version and ships hub schema
/// changes without re-running `yarn refresh-gateway` — every
/// `tools install` silently fails: the sidecar exits 0 with a warn, the
/// desktop sees no binary on disk and reports "install failed".
/// This check turns that silent runtime failure into a loud
/// build-time failure.
///
/// Strategy: exec the bundled sidecar with `--version` and compare. CI
/// matrix builds the sidecar for the runner's own target, and
/// `refresh-gateway.sh` builds for host, so the binary is always
/// host-executable at desktop-build time.
///
/// Escape hatches:
///   - RSCLAW_SKIP_SIDECAR_CHECK=1 — bypass entirely (CI stages that place the
///     sidecar after this crate is configured).
///   - Sidecar reports "dev" — env var was unset during sidecar build; warn and
///     skip (common for ad-hoc `cargo build --bin rsclaw`).
///   - Exec fails (true cross-compile) — warn and skip.
fn check_sidecar_version() {
    if std::env::var("RSCLAW_SKIP_SIDECAR_CHECK").is_ok() {
        return;
    }

    let target = match std::env::var("TARGET") {
        Ok(t) => t,
        Err(_) => return,
    };

    let exe_ext = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sidecar = manifest_dir
        .join("binaries")
        .join(format!("rsclaw-{target}{exe_ext}"));

    println!("cargo:rerun-if-changed={}", sidecar.display());
    println!("cargo:rerun-if-env-changed=RSCLAW_SKIP_SIDECAR_CHECK");

    if !sidecar.exists() {
        println!(
            "cargo:warning=sidecar not found at {} — run `yarn refresh-gateway`",
            sidecar.display()
        );
        return;
    }

    let expected = env!("CARGO_PKG_VERSION");

    let output = match Command::new(&sidecar).arg("--version").output() {
        Ok(o) => o,
        Err(e) => {
            println!(
                "cargo:warning=could not exec sidecar to verify version ({e}); skipping check"
            );
            return;
        }
    };

    if !output.status.success() {
        println!(
            "cargo:warning=sidecar --version exited non-zero ({}); skipping check",
            output.status
        );
        return;
    }

    // Output format: `rsclaw 2026.5.20\n`
    let stdout = String::from_utf8_lossy(&output.stdout);
    let actual = stdout
        .split_whitespace()
        .nth(1)
        .map(str::trim)
        .unwrap_or("");

    if actual == "dev" {
        println!(
            "cargo:warning=sidecar reports version=\"dev\" — skipping match check (cargo build without RSCLAW_BUILD_VERSION)"
        );
        return;
    }

    // Normalise a leading `v`: the release pipelines bake the raw git tag
    // (`v2026.6.16` for CLI, `app-v2026.6.16` → `v2026.6.16` for desktop) into
    // the sidecar's `--version`, while CARGO_PKG_VERSION is always bare
    // (`2026.6.16`). Compare on the bare version so the tag's `v` prefix
    // doesn't spuriously trip the mismatch guard (the recurring desktop
    // release failure).
    if actual.trim_start_matches('v') != expected.trim_start_matches('v') {
        panic!(
            "\n\
             \n\
             ===== bundled sidecar version mismatch =====\n\
             sidecar:  {} (version {actual})\n\
             desktop:  rsclaw-ui {expected} (CARGO_PKG_VERSION)\n\
             \n\
             The bundled rsclaw is stale. Shipping now would produce a desktop\n\
             that talks to the wrong hub schema and silently fails every\n\
             tools/skills/plugins install at runtime.\n\
             \n\
             Fix:\n\
               yarn --cwd ui refresh-gateway\n\
             \n\
             Or bypass (CI only):\n\
               RSCLAW_SKIP_SIDECAR_CHECK=1 cargo build\n\
             ============================================\n\
             \n",
            sidecar.display(),
        );
    }
}
