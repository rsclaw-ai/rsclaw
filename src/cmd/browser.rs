use anyhow::Result;
use serde_json::json;

use crate::cli::browser::BrowserCommand;

/// Handle `rsclaw browser` subcommands.
///
/// CLI output convention (matches agent-browser):
/// - stdout: pure data (text, HTML, JSON values)
/// - stderr: status messages (Connected, Navigated, etc.)
pub async fn cmd_browser(sub: BrowserCommand) -> Result<()> {
    // Try to connect to existing Chrome (remote debugging), otherwise launch new.
    let ports = &[9222_u16, 9223];
    let mut session = if let Some(ws_url) = crate::browser::detect_existing_chrome(ports).await {
        eprintln!("Connected to existing Chrome");
        crate::browser::BrowserSession::connect_existing_reuse(&ws_url).await?
    } else {
        let chrome_path = crate::agent::platform::detect_chrome()
            .ok_or_else(|| anyhow::anyhow!("Chrome not found. Install with: rsclaw tools install chrome"))?;
        eprintln!("Launching headless Chrome");
        crate::browser::BrowserSession::start(&chrome_path, false, None).await?
    };

    let (action, args) = match sub {
        BrowserCommand::Open { url } => ("open", json!({"url": url})),
        BrowserCommand::Snapshot { interactive } => ("snapshot", json!({"interactive": interactive})),
        BrowserCommand::Click { eref } => ("click", json!({"ref": eref})),
        BrowserCommand::ClickAt { eref, x, y } => {
            let mut a = json!({});
            if let Some(r) = eref { a["ref"] = json!(r); }
            if let Some(xv) = x { a["x"] = json!(xv); }
            if let Some(yv) = y { a["y"] = json!(yv); }
            ("clickAt", a)
        }
        BrowserCommand::Fill { eref, text } => ("fill", json!({"ref": eref, "text": text})),
        BrowserCommand::Press { key } => ("press", json!({"key": key})),
        BrowserCommand::Scroll { direction, amount } => ("scroll", json!({"direction": direction, "amount": amount})),
        BrowserCommand::Screenshot { path } => ("screenshot", json!({"path": path})),
        BrowserCommand::Text => ("get_text", json!({})),
        BrowserCommand::Url => ("get_url", json!({})),
        BrowserCommand::Title => ("get_title", json!({})),
        BrowserCommand::Content => ("content", json!({})),
        BrowserCommand::Console { limit } => ("console", json!({"limit": limit})),
        BrowserCommand::Wait { target, timeout } => ("wait", json!({"target": target, "timeout": timeout})),
        BrowserCommand::WaitForUrl { pattern, timeout } => ("waitforurl", json!({"url": pattern, "timeout": timeout})),
        BrowserCommand::Evaluate { js } => ("evaluate", json!({"js": js})),
        BrowserCommand::GetByText { text, exact } => ("getbytext", json!({"value": text, "exact": exact})),
        BrowserCommand::GetByRole { role } => ("getbyrole", json!({"value": role})),
        BrowserCommand::GetByLabel { label } => ("getbylabel", json!({"value": label})),
        BrowserCommand::Find { text } => ("find", json!({"text": text})),
        BrowserCommand::Back => ("back", json!({})),
        BrowserCommand::Forward => ("forward", json!({})),
        BrowserCommand::Reload => ("reload", json!({})),
        BrowserCommand::Raw { action, args } => {
            let parsed: serde_json::Value = serde_json::from_str(&args).unwrap_or(json!({}));
            (action.leak() as &str, parsed)
        }
    };

    // For screenshot: extract save path before execute.
    let screenshot_path = if action == "screenshot" {
        args.get("path").and_then(|v| v.as_str()).map(String::from)
    } else {
        None
    };

    let result = session.execute(action, &args).await?;

    // CLI output: pure data to stdout, status to stderr.
    match action {
        "screenshot" => {
            if let Some(path) = screenshot_path {
                if let Some(data_uri) = result.get("image").and_then(|v| v.as_str()) {
                    let b64 = data_uri.split(',').nth(1).unwrap_or(data_uri);
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD.decode(b64)
                        .map_err(|e| anyhow::anyhow!("failed to decode screenshot: {e}"))?;
                    std::fs::write(&path, &bytes)?;
                    eprintln!("Screenshot saved to {path} ({} bytes)", bytes.len());
                }
            }
        }
        "open" | "navigate" => {
            // Status only — navigated URL to stderr.
            if let Some(url) = result.get("url").and_then(|v| v.as_str()) {
                eprintln!("Navigated to {url}");
            }
        }
        "snapshot" => {
            // Raw snapshot text to stdout.
            if let Some(text) = result.get("text").and_then(|v| v.as_str()) {
                print!("{text}");
            }
        }
        "get_text" => {
            if let Some(text) = result.get("text").and_then(|v| v.as_str()) {
                print!("{text}");
            }
        }
        "get_url" => {
            if let Some(url) = result.get("url").and_then(|v| v.as_str()) {
                print!("{url}");
            }
        }
        "get_title" => {
            if let Some(title) = result.get("title").and_then(|v| v.as_str()) {
                print!("{title}");
            }
        }
        "content" => {
            if let Some(html) = result.get("html").and_then(|v| v.as_str()) {
                print!("{html}");
            }
        }
        "evaluate" => {
            // Raw JS return value — string without JSON wrapper.
            if let Some(val) = result.get("result") {
                match val {
                    serde_json::Value::String(s) => print!("{s}"),
                    other => print!("{}", serde_json::to_string_pretty(other).unwrap_or_default()),
                }
            }
        }
        // click, fill, press, scroll, etc. — status to stderr.
        "click" | "clickAt" | "fill" | "press" | "scroll" | "back" | "forward" | "reload"
        | "check" | "uncheck" | "hover" | "focus" | "dialog" => {
            if let Some(action_name) = result.get("action").and_then(|v| v.as_str()) {
                eprintln!("{action_name}: ok");
            }
        }
        // Everything else: JSON to stdout.
        _ => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }

    Ok(())
}
