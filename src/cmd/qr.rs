use anyhow::Result;

use crate::{cli::QrArgs, config};

pub async fn cmd_qr(args: QrArgs) -> Result<()> {
    let cfg = config::load().ok();
    let port = cfg.as_ref().map_or(18888, |c| c.gateway.port);
    let default_url = format!("http://127.0.0.1:{port}");

    let url = if args.remote {
        args.public_url
            .clone()
            .unwrap_or_else(|| default_url.clone())
    } else {
        args.url.clone().unwrap_or(default_url)
    };

    let auth_token = args
        .token
        .clone()
        .or_else(|| cfg.as_ref().and_then(|c| c.gateway.auth_token.clone()));

    // Build connection payload.
    let payload = if let Some(ref pw) = args.password {
        serde_json::json!({
            "url": url,
            "password": pw,
        })
    } else {
        serde_json::json!({
            "url": url,
            "token": auth_token.as_deref().unwrap_or(""),
        })
    };

    let payload_str = serde_json::to_string(&payload)?;

    if args.setup_code_only {
        println!("{payload_str}");
        return Ok(());
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if args.no_ascii {
        // Just print the payload for copy-paste.
        println!("payload: {payload_str}");
    } else {
        println!("scan this QR code with the OpenClaw iOS app:");
        println!();
        crate::channel::auth::display_qr_terminal(&payload_str)?;
    }

    Ok(())
}
