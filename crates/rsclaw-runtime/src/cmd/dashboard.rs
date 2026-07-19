use anyhow::Result;
use rsclaw_config as config;

pub async fn cmd_dashboard(no_open: bool) -> Result<()> {
    let cfg = config::load().ok();
    let port = cfg.as_ref().map_or(18888, |c| c.gateway.port);
    let auth_token = cfg
        .as_ref()
        .and_then(|c| c.gateway.auth_token.clone())
        .unwrap_or_default();

    let url = if auth_token.is_empty() {
        format!("http://127.0.0.1:{port}/")
    } else {
        format!("http://127.0.0.1:{port}/?token={auth_token}")
    };

    if no_open {
        println!("{url}");
    } else {
        println!("opening Control UI: {url}");
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg(&url).spawn();
        }
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
        }
        #[cfg(target_os = "windows")]
        {
            // CREATE_NO_WINDOW = 0x08000000 — see `cmd/docs.rs` for the
            // same fix; cmd /C flashes briefly without it.
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", &url])
                .creation_flags(0x08000000)
                .spawn();
        }
    }

    Ok(())
}
