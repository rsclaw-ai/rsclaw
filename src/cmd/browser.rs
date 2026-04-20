use anyhow::Result;
use serde_json::json;

use crate::cli::browser::BrowserCommand;

/// Handle `rsclaw browser` subcommands.
pub async fn cmd_browser(sub: BrowserCommand) -> Result<()> {
    // Try to connect to existing Chrome (remote debugging), otherwise launch new.
    let ports = &[9222_u16, 9223];
    let mut session = if let Some(ws_url) = crate::browser::detect_existing_chrome(ports).await {
        eprintln!("Connected to existing Chrome");
        crate::browser::BrowserSession::connect_existing(&ws_url).await?
    } else {
        let chrome_path = crate::agent::platform::detect_chrome()
            .ok_or_else(|| anyhow::anyhow!("Chrome not found. Start Chrome with --remote-debugging-port=9222 or install Chrome."))?;
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
        BrowserCommand::Back => ("back", json!({})),
        BrowserCommand::Forward => ("forward", json!({})),
        BrowserCommand::Reload => ("reload", json!({})),
        BrowserCommand::Raw { action, args } => {
            let parsed: serde_json::Value = serde_json::from_str(&args).unwrap_or(json!({}));
            (action.leak() as &str, parsed)
        }
    };

    let result = session.execute(action, &args).await?;

    // Pretty print result.
    if let Some(text) = result.get("text").and_then(|v| v.as_str()) {
        // Snapshot/text output: print raw text.
        println!("{text}");
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }

    Ok(())
}
