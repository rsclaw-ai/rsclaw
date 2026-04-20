use anyhow::{Result, anyhow};
use serde_json::json;

use crate::cli::browser::{AuthCommand, BrowserCommand, GetCommand, KeyboardCommand, TabCommand};

/// Handle `rsclaw browser` subcommands.
///
/// CLI output convention (matches agent-browser):
/// - stdout: pure data (text, HTML, JSON values)
/// - stderr: status messages (Connected, Navigated, etc.)
pub async fn cmd_browser(sub: BrowserCommand) -> Result<()> {
    // Batch is special: it manages its own session.
    if let BrowserCommand::Batch { commands } = sub {
        return cmd_batch(commands).await;
    }

    let mut session = connect_or_launch(None).await?;
    dispatch_and_print(&mut session, sub).await
}

/// Connect to an existing Chrome instance or launch a new one.
async fn connect_or_launch(port: Option<u16>) -> Result<crate::browser::BrowserSession> {
    let ports: Vec<u16> = if let Some(p) = port {
        vec![p]
    } else {
        vec![9222, 9223]
    };
    if let Some(ws_url) = crate::browser::detect_existing_chrome(&ports).await {
        tracing::debug!("Connected to existing Chrome");
        crate::browser::BrowserSession::connect_existing_reuse(&ws_url).await
    } else {
        let chrome_path = crate::agent::platform::detect_chrome()
            .ok_or_else(|| anyhow!("Chrome not found. Install with: rsclaw tools install chrome"))?;
        let profile = std::env::var("RSCLAW_BROWSER_PROFILE").ok();
        let headed = crate::agent::platform::has_display();
        tracing::debug!(headed, "Launching Chrome");
        crate::browser::BrowserSession::start(&chrome_path, headed, profile.as_deref()).await
    }
}

/// Execute a batch of commands sequentially on the same session.
async fn cmd_batch(commands: Vec<String>) -> Result<()> {
    use clap::FromArgMatches;

    let mut session = connect_or_launch(None).await?;

    // Build a clap Command that matches BrowserCommand's subcommands.
    let cli_cmd = <BrowserCommand as clap::Subcommand>::augment_subcommands(
        clap::Command::new("browser"),
    );

    for (i, cmd_str) in commands.iter().enumerate() {
        let words = shell_words(cmd_str);
        let word_refs: Vec<&str> = std::iter::once("browser")
            .chain(words.iter().map(|s| s.as_str()))
            .collect();

        let matches = cli_cmd
            .clone()
            .try_get_matches_from(&word_refs)
            .map_err(|e| anyhow!("batch[{i}]: failed to parse '{cmd_str}': {e}"))?;

        let parsed = BrowserCommand::from_arg_matches(&matches)
            .map_err(|e| anyhow!("batch[{i}]: failed to parse '{cmd_str}': {e}"))?;

        // Nested batch is not allowed.
        if matches!(parsed, BrowserCommand::Batch { .. }) {
            return Err(anyhow!("nested batch commands are not supported"));
        }

        eprintln!("batch[{i}]: {cmd_str}");
        dispatch_and_print(&mut session, parsed).await?;
    }

    Ok(())
}

/// Simple shell-word splitting (handles double quotes).
fn shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => in_quote = !in_quote,
            ' ' | '\t' if !in_quote => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Translate a `BrowserCommand` to an action + args, execute, and print results.
async fn dispatch_and_print(
    session: &mut crate::browser::BrowserSession,
    sub: BrowserCommand,
) -> Result<()> {
    // Commands that don't go through execute().
    match &sub {
        BrowserCommand::Close => {
            drop(session);
            eprintln!("Browser closed");
            return Ok(());
        }
        BrowserCommand::Profiles => {
            return cmd_profiles();
        }
        BrowserCommand::Auth(auth_cmd) => {
            return cmd_auth(auth_cmd).await;
        }
        BrowserCommand::Connect { target } => {
            if target.starts_with("ws://") || target.starts_with("wss://") {
                // Direct WebSocket URL.
                let _session = crate::browser::BrowserSession::connect_existing_reuse(target).await?;
                eprintln!("Connected to Chrome via {target}");
            } else if let Ok(port) = target.parse::<u16>() {
                let ports = [port];
                let ws_url = crate::browser::detect_existing_chrome(&ports)
                    .await
                    .ok_or_else(|| anyhow!("no Chrome found on port {port}"))?;
                eprintln!("Connected to Chrome on port {port}: {ws_url}");
            } else {
                return Err(anyhow!("connect: expected port number or ws:// URL, got '{target}'"));
            }
            return Ok(());
        }
        BrowserCommand::StateSave { path } => {
            // Save cookies + localStorage to JSON file.
            let cookies = session.execute("cookies", &json!({"value": "get"})).await?;
            let storage = session.execute("storage", &json!({"value": "get", "type": "local"})).await?;
            let state = json!({
                "cookies": cookies.get("cookies").cloned().unwrap_or(json!([])),
                "localStorage": storage.get("data").cloned().unwrap_or(json!({})),
                "url": session.execute("get_url", &json!({})).await.ok()
                    .and_then(|r| r.get("url").and_then(|v| v.as_str()).map(String::from))
                    .unwrap_or_default(),
            });
            std::fs::write(path, serde_json::to_string_pretty(&state)?)?;
            eprintln!("State saved to {path}");
            return Ok(());
        }
        BrowserCommand::DownloadVideo { url, output, wait } => {
            // 1. Capture video URLs.
            eprintln!("Capturing video URLs from {url}...");
            let result = session.execute("capture_video", &json!({"url": url, "wait_ms": wait})).await?;
            let urls = result.get("video_urls")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            if urls.is_empty() {
                eprintln!("No video URLs found. Try logging in first (rsclaw browser state-load).");
                return Ok(());
            }

            // 2. Pick the best URL — filter out noise, prefer real video streams.
            let bad_patterns = ["static", "douyinstatic", "poster", "cover", "thumbnail",
                "preview", "placeholder", "loading", "uuu_", "ad.", "advert"];
            let best = urls.iter()
                .filter_map(|u| u.as_str())
                .filter(|u| !u.contains("audio"))
                .filter(|u| !bad_patterns.iter().any(|p| u.contains(p)))
                .max_by_key(|u| {
                    let mut score = 0i32;
                    if u.contains("playaddr") || u.contains("play_addr") { score += 20; }
                    if u.contains("douyinvod") || u.contains("bilivideo") { score += 15; }
                    if u.contains(".mp4") { score += 10; }
                    if u.contains(".m4s") { score += 8; }
                    if u.contains("1080") { score += 3; }
                    if u.contains("720") { score += 2; }
                    score
                })
                .or_else(|| urls.first().and_then(|u| u.as_str()));

            let Some(video_url) = best else {
                eprintln!("No suitable video URL found.");
                return Ok(());
            };

            eprintln!("Downloading: {}", &video_url[..video_url.len().min(100)]);

            // 3. Extract cookies from browser for this domain.
            let cookies_result = session.execute("cookies", &json!({"value": "get"})).await.ok();
            let cookie_header = cookies_result
                .as_ref()
                .and_then(|r| r.get("cookies"))
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| {
                            let name = c.get("name").and_then(|v| v.as_str())?;
                            let value = c.get("value").and_then(|v| v.as_str())?;
                            Some(format!("{name}={value}"))
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default();

            // 4. Download with cookies and referer.
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()?;

            let referer = if video_url.contains("bilibili") || video_url.contains("bilivideo") {
                "https://www.bilibili.com/"
            } else if video_url.contains("douyin") || video_url.contains("douyinvod") {
                "https://www.douyin.com/"
            } else {
                &url
            };

            let resp = client.get(video_url)
                .header("Cookie", &cookie_header)
                .header("Referer", referer)
                .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
                .send()
                .await?;

            if !resp.status().is_success() {
                eprintln!("Download failed: HTTP {}", resp.status());
                return Ok(());
            }

            let bytes = resp.bytes().await?;
            std::fs::write(&output, &bytes)?;
            eprintln!("Saved to {output} ({} bytes)", bytes.len());
            return Ok(());
        }
        BrowserCommand::Requests { clear, filter } => {
            let js = "JSON.stringify(performance.getEntriesByType('resource').map(e => ({name: e.name, type: e.initiatorType, duration: Math.round(e.duration), size: e.transferSize || 0})))";
            let result = session.execute("evaluate", &json!({"js": js})).await?;
            if let Some(val) = result.get("result") {
                let text = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                };
                if let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                    let filtered: Vec<&serde_json::Value> = if let Some(pat) = filter {
                        entries.iter().filter(|e| {
                            e.get("name").and_then(|v| v.as_str())
                                .map(|n| n.contains(pat.as_str()))
                                .unwrap_or(false)
                        }).collect()
                    } else {
                        entries.iter().collect()
                    };
                    println!("{}", serde_json::to_string_pretty(&filtered)?);
                } else {
                    println!("{text}");
                }
            }
            if *clear {
                let _ = session.execute("evaluate", &json!({"js": "performance.clearResourceTimings()"})).await;
                eprintln!("Resource timings cleared");
            }
            return Ok(());
        }
        BrowserCommand::Session { action } => {
            match action.as_str() {
                "list" => {
                    // List all Chrome debugging targets via /json endpoint.
                    let port = session.debug_port();
                    let url = format!("http://127.0.0.1:{port}/json");
                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .build()?;
                    let resp = client.get(&url).send().await?;
                    let targets: serde_json::Value = resp.json().await?;
                    println!("{}", serde_json::to_string_pretty(&targets)?);
                }
                _ => {
                    // "show" -- print current session info.
                    let url_result = session.execute("get_url", &json!({})).await.ok();
                    let title_result = session.execute("get_title", &json!({})).await.ok();
                    let tabs_result = session.execute("list_tabs", &json!({})).await.ok();
                    let url = url_result.as_ref()
                        .and_then(|r| r.get("url")).and_then(|v| v.as_str()).unwrap_or("unknown");
                    let title = title_result.as_ref()
                        .and_then(|r| r.get("title")).and_then(|v| v.as_str()).unwrap_or("unknown");
                    let tab_count = tabs_result.as_ref()
                        .and_then(|r| r.get("tabs")).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                    let info = json!({
                        "url": url,
                        "title": title,
                        "tabs": tab_count,
                    });
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
            }
            return Ok(());
        }
        BrowserCommand::StateLoad { path } => {
            // Load cookies + localStorage from JSON file.
            let content = std::fs::read_to_string(path)
                .map_err(|e| anyhow!("failed to read {path}: {e}"))?;
            let state: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| anyhow!("invalid state file: {e}"))?;

            // Restore cookies.
            if let Some(cookies) = state.get("cookies").and_then(|v| v.as_array()) {
                for cookie in cookies {
                    let _ = session.execute("cookies", &json!({"value": "set", "cookie": cookie})).await;
                }
            }

            // Navigate to saved URL to apply cookies, then restore localStorage.
            if let Some(url) = state.get("url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    let _ = session.execute("open", &json!({"url": url})).await;
                }
            }

            if let Some(storage) = state.get("localStorage").and_then(|v| v.as_object()) {
                for (k, v) in storage {
                    let val = v.as_str().unwrap_or("");
                    let js = format!(
                        "localStorage.setItem('{}', '{}')",
                        k.replace('\'', "\\'"),
                        val.replace('\'', "\\'")
                    );
                    let _ = session.execute("evaluate", &json!({"js": js})).await;
                }
            }

            eprintln!("State loaded from {path}");
            return Ok(());
        }
        _ => {}
    }

    let (action, args) = to_action_args(sub);

    // For screenshot: extract save path before execute.
    let screenshot_path = if action == "screenshot" || action == "annotate" {
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
                        .map_err(|e| anyhow!("failed to decode screenshot: {e}"))?;
                    std::fs::write(&path, &bytes)?;
                    eprintln!("Screenshot saved to {path} ({} bytes)", bytes.len());
                }
            }
        }
        "annotate" => {
            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                if let Some(data_uri) = result.get("image").and_then(|v| v.as_str()) {
                    let b64 = data_uri.split(',').nth(1).unwrap_or(data_uri);
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD.decode(b64)
                        .map_err(|e| anyhow!("failed to decode: {e}"))?;
                    std::fs::write(path, &bytes)?;
                    let labels = result.get("labels").and_then(|v| v.as_u64()).unwrap_or(0);
                    eprintln!("Annotated screenshot saved to {path} ({labels} labels)");
                }
            }
        }
        "inspect" => {
            if let Some(url) = result.get("devtools_url").and_then(|v| v.as_str()) {
                println!("{url}");
            }
        }
        "open" | "navigate" => {
            if let Some(url) = result.get("url").and_then(|v| v.as_str()) {
                eprintln!("Navigated to {url}");
            }
        }
        "snapshot" => {
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
            if let Some(val) = result.get("result") {
                match val {
                    serde_json::Value::String(s) => print!("{s}"),
                    other => print!("{}", serde_json::to_string_pretty(other).unwrap_or_default()),
                }
            }
        }
        // click, fill, press, scroll, etc. -- status to stderr.
        "click" | "clickAt" | "fill" | "press" | "scroll" | "back" | "forward" | "reload"
        | "check" | "uncheck" | "hover" | "focus" | "dialog"
        | "new_tab" | "switch_tab" | "close_tab" => {
            if let Some(action_name) = result.get("action").and_then(|v| v.as_str()) {
                eprintln!("{action_name}: ok");
            } else {
                eprintln!("{action}: ok");
            }
        }
        // Everything else: JSON to stdout.
        _ => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }

    Ok(())
}

/// Map `BrowserCommand` variants to (action, args) for `session.execute()`.
///
/// Returns a `&'static str` action and a `serde_json::Value` args object.
fn to_action_args(sub: BrowserCommand) -> (&'static str, serde_json::Value) {
    match sub {
        BrowserCommand::Open { url } => ("open", json!({"url": url})),
        BrowserCommand::Snapshot { interactive, compact, depth, selector } => ("snapshot", json!({
            "interactive": interactive,
            "compact": compact,
            "depth": depth,
            "selector": selector,
        })),
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
        BrowserCommand::Annotate { path } => ("annotate", json!({"path": path})),
        BrowserCommand::Inspect => ("inspect", json!({})),
        BrowserCommand::Text => ("get_text", json!({})),
        BrowserCommand::Url => ("get_url", json!({})),
        BrowserCommand::Title => ("get_title", json!({})),
        BrowserCommand::Content => ("content", json!({})),
        BrowserCommand::Console { limit } => ("console", json!({"limit": limit})),
        BrowserCommand::Wait { target, timeout } => ("wait", json!({"target": target, "timeout": timeout})),
        BrowserCommand::WaitForUrl { pattern, timeout } => ("waitforurl", json!({"url": pattern, "timeout": timeout})),
        BrowserCommand::Evaluate { js } => ("evaluate", json!({"js": js})),
        BrowserCommand::CaptureVideo { url, wait } => ("capture_video", json!({"url": url, "wait_ms": wait})),
        BrowserCommand::GetByText { text, exact } => ("getbytext", json!({"value": text, "exact": exact})),
        BrowserCommand::GetByRole { role } => ("getbyrole", json!({"value": role})),
        BrowserCommand::GetByLabel { label } => ("getbylabel", json!({"value": label})),
        BrowserCommand::Find { text } => ("find", json!({"text": text})),
        BrowserCommand::Back => ("back", json!({})),
        BrowserCommand::Forward => ("forward", json!({})),
        BrowserCommand::Reload => ("reload", json!({})),
        BrowserCommand::Tab(tab) => match tab {
            TabCommand::New { url } => ("new_tab", json!({"url": url.unwrap_or_default()})),
            TabCommand::List => ("list_tabs", json!({})),
            TabCommand::Close { index } => ("close_tab", json!({"index": index})),
            TabCommand::Switch { index } => ("switch_tab", json!({"index": index})),
        },
        BrowserCommand::Get(get) => match get {
            GetCommand::Text { selector } => ("get", json!({"what": "text", "selector": selector})),
            GetCommand::Html { selector } => ("get", json!({"what": "html", "selector": selector})),
            GetCommand::Value { selector } => ("get", json!({"what": "value", "selector": selector})),
            GetCommand::Attr { name, selector } => ("get", json!({"what": "attr", "name": name, "selector": selector})),
            GetCommand::Count { selector } => ("get", json!({"what": "count", "selector": selector})),
            GetCommand::Box { selector } => ("get", json!({"what": "box", "selector": selector})),
        },
        BrowserCommand::Errors => ("evaluate", json!({"js": "(window.__rsclaw_errors || []).map(e => e.toString()).join('\\n')"})),
        BrowserCommand::Keyboard(kb) => match kb {
            KeyboardCommand::Type { text } => ("evaluate", json!({
                "js": format!(
                    "void(await (async () => {{ for (const ch of {}) {{ await new Promise(r => setTimeout(r, 30)); document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', {{key: ch}})); document.activeElement?.dispatchEvent(new KeyboardEvent('keypress', {{key: ch}})); document.execCommand('insertText', false, ch); document.activeElement?.dispatchEvent(new KeyboardEvent('keyup', {{key: ch}})); }} }})())",
                    serde_json::to_string(&text).unwrap_or_default()
                )
            })),
            KeyboardCommand::Inserttext { text } => ("evaluate", json!({
                "js": format!("document.execCommand('insertText', false, {})", serde_json::to_string(&text).unwrap_or_default())
            })),
        },
        BrowserCommand::Download { selector, path } => ("download_wait", json!({"selector": selector, "path": path})),
        BrowserCommand::Raw { action, args } => {
            let parsed: serde_json::Value = serde_json::from_str(&args).unwrap_or(json!({}));
            (action.leak() as &str, parsed)
        }
        // These are handled before reaching to_action_args.
        BrowserCommand::Close
        | BrowserCommand::Batch { .. }
        | BrowserCommand::StateSave { .. }
        | BrowserCommand::StateLoad { .. }
        | BrowserCommand::Auth(_)
        | BrowserCommand::Profiles
        | BrowserCommand::Connect { .. }
        | BrowserCommand::Requests { .. }
        | BrowserCommand::Session { .. }
        | BrowserCommand::DownloadVideo { .. } => unreachable!(),
    }
}

/// List available Chrome profile directories.
fn cmd_profiles() -> Result<()> {
    let home = dirs_next::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;

    #[cfg(target_os = "macos")]
    let chrome_dir = home.join("Library/Application Support/Google/Chrome");
    #[cfg(target_os = "linux")]
    let chrome_dir = home.join(".config/google-chrome");
    #[cfg(target_os = "windows")]
    let chrome_dir = home.join("AppData/Local/Google/Chrome/User Data");

    if !chrome_dir.exists() {
        eprintln!("Chrome config directory not found: {}", chrome_dir.display());
        return Ok(());
    }

    let mut profiles = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&chrome_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "Default" || name.starts_with("Profile ") {
                let prefs_path = entry.path().join("Preferences");
                let display_name = if prefs_path.exists() {
                    std::fs::read_to_string(&prefs_path)
                        .ok()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                        .and_then(|v| v["profile"]["name"].as_str().map(String::from))
                        .unwrap_or_else(|| name.clone())
                } else {
                    name.clone()
                };
                profiles.push(json!({"dir": name, "name": display_name}));
            }
        }
    }

    println!("{}", serde_json::to_string_pretty(&profiles)?);
    Ok(())
}

/// Handle auth vault subcommands.
async fn cmd_auth(auth_cmd: &AuthCommand) -> Result<()> {
    let vault_path = crate::config::loader::base_dir().join("auth-vault.json");

    match auth_cmd {
        AuthCommand::Save { site, username, password } => {
            let mut vault = load_vault(&vault_path)?;
            vault[site.as_str()] = json!({
                "username": username,
                "password": password,
            });
            save_vault(&vault_path, &vault)?;
            eprintln!("Saved credentials for {site}");
        }
        AuthCommand::Login { site } => {
            let vault = load_vault(&vault_path)?;
            let entry = vault.get(site.as_str())
                .ok_or_else(|| anyhow!("no credentials found for {site}"))?;
            println!("{}", serde_json::to_string_pretty(entry)?);
            eprintln!("Use the returned credentials to fill login forms");
        }
        AuthCommand::List => {
            let vault = load_vault(&vault_path)?;
            if let Some(obj) = vault.as_object() {
                let sites: Vec<&String> = obj.keys().collect();
                println!("{}", serde_json::to_string_pretty(&sites)?);
            }
        }
        AuthCommand::Show { site } => {
            let vault = load_vault(&vault_path)?;
            let entry = vault.get(site.as_str())
                .ok_or_else(|| anyhow!("no credentials found for {site}"))?;
            println!("{}", serde_json::to_string_pretty(entry)?);
        }
        AuthCommand::Delete { site } => {
            let mut vault = load_vault(&vault_path)?;
            if let Some(obj) = vault.as_object_mut() {
                if obj.remove(site.as_str()).is_some() {
                    save_vault(&vault_path, &vault)?;
                    eprintln!("Deleted credentials for {site}");
                } else {
                    return Err(anyhow!("no credentials found for {site}"));
                }
            }
        }
    }
    Ok(())
}

/// Load the auth vault JSON file, returning an empty object if missing.
fn load_vault(path: &std::path::Path) -> Result<serde_json::Value> {
    if path.exists() {
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data)?)
    } else {
        Ok(json!({}))
    }
}

/// Save the auth vault JSON file.
fn save_vault(path: &std::path::Path, vault: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(vault)?)?;
    Ok(())
}
