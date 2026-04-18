//! Fast preparse — local slash-command handling that bypasses the agent queue.
//!
//! Functions here handle commands like `/ls`, `/status`, `/version`, `/btw`
//! without going through the LLM agent loop.

use std::time::Duration;

use futures::StreamExt as _;
use tokio::sync::mpsc;
use tracing::warn;

use crate::{
    agent::LiveStatus,
    channel::OutboundMessage,
    config::runtime::RuntimeConfig,
    provider::{
        LlmRequest, Message, MessageContent, Role, StreamEvent,
        failover::FailoverManager,
        registry::ProviderRegistry,
    },
};

/// Handle certain fast preparse commands locally — without going through the agent queue.
/// Returns `Some(reply_text)` for commands that can be answered immediately, `None` otherwise.
/// This avoids blocking on the agent's sequential LLM loop for simple commands like /ls, /status.
pub(crate) async fn try_preparse_locally(
    text: &str,
    handle: &crate::agent::AgentHandle,
) -> Option<OutboundMessage> {
    use std::sync::atomic::Ordering;
    let t = text.trim();
    let lower = t.to_lowercase();

    // Helper: text-only reply (target_id/is_group filled in by caller).
    let txt = |s: String| OutboundMessage {
        target_id: String::new(),
        is_group: false,
        text: s,
        reply_to: None,
        images: vec![],
        files: vec![],
        channel: None,
    };

    // Workspace resolver (shared by /ls, /cat, shell cmds).
    let workspace = || {
        let base = crate::config::loader::base_dir();
        handle.config.workspace.as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| base.join("workspace"))
    };

    // /version
    if lower == "/version" {
        return Some(txt(format!("rsclaw v{}", env!("RSCLAW_BUILD_VERSION"))));
    }
    // /health
    if lower == "/health" {
        let secs = handle.started_at.elapsed().as_secs();
        let uptime = if secs < 60 { format!("{secs}s") }
            else if secs < 3600 { format!("{}m {}s", secs/60, secs%60) }
            else { format!("{}h {}m", secs/3600, (secs%3600)/60) };
        return Some(txt(format!("OK · up {uptime}")));
    }
    // /uptime
    if lower == "/uptime" {
        let secs = handle.started_at.elapsed().as_secs();
        let s = if secs < 60 { format!("{secs}s") }
            else if secs < 3600 { format!("{}m {}s", secs/60, secs%60) }
            else { format!("{}h {}m", secs/3600, (secs%3600)/60) };
        return Some(txt(s));
    }
    // /abort — set all abort flags
    if lower == "/abort" {
        let flags = handle.abort_flags.read().unwrap();
        let count = flags.len();
        for f in flags.values() { f.store(true, Ordering::SeqCst); }
        return Some(txt(if count > 0 { format!("✓ abort signal sent ({count} session(s))") } else { "nothing to abort".to_owned() }));
    }
    // /clear — abort running turns + signal session clear (fully non-blocking)
    if lower == "/clear" {
        // 1. Abort all running turns
        let flags = handle.abort_flags.read().unwrap();
        for f in flags.values() { f.store(true, Ordering::SeqCst); }
        drop(flags);
        // 2. Signal runtime to clear sessions at next opportunity
        handle.clear_signal.store(true, Ordering::SeqCst);
        return Some(txt("✓ Session cleared.".to_owned()));
    }
    // /new — start a fresh conversation (new generation, no summary)
    if lower == "/new" {
        let flags = handle.abort_flags.read().unwrap();
        for f in flags.values() { f.store(true, Ordering::SeqCst); }
        drop(flags);
        handle.new_session_signal.store(true, Ordering::SeqCst);
        return Some(txt("✓ New session started.".to_owned()));
    }
    // /reset — reset current session (no summary, same generation)
    if lower == "/reset" {
        let flags = handle.abort_flags.read().unwrap();
        for f in flags.values() { f.store(true, Ordering::SeqCst); }
        drop(flags);
        handle.reset_signal.store(true, Ordering::SeqCst);
        return Some(txt("✓ Session reset.".to_owned()));
    }
    // /status
    if lower == "/status" {
        let model = handle.config.model.as_ref()
            .and_then(|m| m.primary.as_deref())
            .unwrap_or("default");
        let sessions = handle.session_count.load(Ordering::Relaxed);
        let secs = handle.started_at.elapsed().as_secs();
        let uptime = if secs < 60 { format!("{secs}s") }
            else if secs < 3600 { format!("{}m {}s", secs/60, secs%60) }
            else { format!("{}h {}m", secs/3600, (secs%3600)/60) };
        let os = if cfg!(target_os = "macos") { "macOS" }
            else if cfg!(target_os = "linux") {
                if std::env::var("ANDROID_ROOT").is_ok() { "Android" } else { "Linux" }
            }
            else if cfg!(target_os = "windows") { "Windows" }
            else { "Unknown" };
        let ctx_tokens = handle.last_ctx_tokens.load(Ordering::Relaxed);
        let ctx_limit = handle.config.model.as_ref()
            .and_then(|m| m.context_tokens)
            .unwrap_or(64000) as usize;
        return Some(txt(format!(
            "Gateway: running\nOS: {os}\nModel: {model}\nSessions: {sessions}\nContext: ~{:.1}k/{:.0}k tokens\nUptime: {uptime}\nVersion: rsclaw v{}",
            ctx_tokens as f64 / 1000.0,
            ctx_limit as f64 / 1000.0,
            env!("RSCLAW_BUILD_VERSION")
        )));
    }
    // /ls [path] — list workspace directory
    if lower == "/ls" || lower.starts_with("/ls ") {
        let path_arg = t.get(3..).unwrap_or("").trim();
        let ws = workspace();
        let target = if path_arg.is_empty() {
            ws
        } else {
            let p = std::path::PathBuf::from(path_arg);
            if p.is_absolute() { p } else { ws.join(path_arg) }
        };
        let out = tokio::process::Command::new("ls")
            .current_dir(&target)
            .output()
            .await
            .ok()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        return Some(txt(if stdout.trim().is_empty() { "(empty directory)".to_owned() } else { stdout }));
    }
    // /cat <path> — read file from workspace
    if lower.starts_with("/cat ") {
        let path_arg = t.get(5..).unwrap_or("").trim();
        let ws = workspace();
        let target = {
            let p = std::path::PathBuf::from(path_arg);
            if p.is_absolute() { p } else { ws.join(path_arg) }
        };
        let content = tokio::fs::read_to_string(&target).await
            .unwrap_or_else(|e| format!("error reading {}: {e}", target.display()));
        return Some(txt(content));
    }
    // /ss — desktop screenshot
    if lower == "/ss" || lower == "/screenshot" {
        let tmp_path = std::env::temp_dir().join("rsclaw_screen.png");
        let tmp_s = tmp_path.to_string_lossy().to_string();
        let ok = if cfg!(target_os = "macos") {
            tokio::process::Command::new("screencapture")
                .args(["-x", &tmp_s]).status().await.map(|s| s.success()).unwrap_or(false)
        } else if cfg!(target_os = "windows") {
            let script = format!(
                r#"Add-Type -AssemblyName System.Windows.Forms,System.Drawing
$b=New-Object System.Drawing.Bitmap([System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width,[System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height)
$g=[System.Drawing.Graphics]::FromImage($b)
$g.CopyFromScreen(0,0,0,0,$b.Size)
$b.Save('{tmp_s}')
$g.Dispose();$b.Dispose()"#
            );
            {
                #[cfg(target_os = "windows")]
                let mut cmd = {
                    use std::os::windows::process::CommandExt;
                    let mut c = tokio::process::Command::new("powershell");
                    c.creation_flags(0x08000000);
                    c
                };
                #[cfg(not(target_os = "windows"))]
                let mut cmd = tokio::process::Command::new("powershell");
                cmd.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
                    .status().await.map(|s| s.success()).unwrap_or(false)
            }
        } else {
            tokio::process::Command::new("scrot")
                .arg(&tmp_s).status().await.map(|s| s.success()).unwrap_or(false)
        };
        if ok {
            if let Ok(bytes) = tokio::fs::read(&tmp_path).await {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                return Some(OutboundMessage {
                    target_id: String::new(),
                    is_group: false,
                    text: String::new(),
                    reply_to: None,
                    images: vec![format!("data:image/png;base64,{b64}")],
                    files: vec![],
                    channel: None,
                });
            }
        }
        return Some(txt("screenshot failed".to_owned()));
    }
    // /skill list — list installed skills (system + agent workspace)
    if lower == "/skill list" {
        let base = crate::config::loader::base_dir();
        let global_dir = base.join("skills");
        let ws_dir = workspace().join("skills");

        let scan = |dir: &std::path::Path| -> Vec<String> {
            let mut names = Vec::new();
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() && p.join("SKILL.md").exists() {
                        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                            names.push(name.to_owned());
                        }
                    }
                }
            }
            names.sort();
            names
        };

        let global = scan(&global_dir);
        let agent = scan(&ws_dir);

        let mut lines = Vec::new();
        lines.push(format!("System skills ({}):", global.len()));
        if global.is_empty() {
            lines.push("  (none)".to_owned());
        } else {
            for s in &global { lines.push(format!("  {s}")); }
        }
        lines.push(format!("Agent skills ({}):", agent.len()));
        if agent.is_empty() {
            lines.push("  (none)".to_owned());
        } else {
            for s in &agent { lines.push(format!("  {s}")); }
        }
        return Some(txt(lines.join("\n")));
    }
    // /cron — list cron jobs (reads from disk)
    if lower == "/cron" || lower == "/cron list" {
        let jobs_path = crate::config::loader::base_dir().join("cron.json5");
        let reply = match tokio::fs::read_to_string(&jobs_path).await {
            Ok(content) => {
                let parsed: Option<Vec<serde_json::Value>> = json5::from_str::<serde_json::Value>(&content)
                    .or_else(|_| serde_json::from_str(&content))
                    .ok()
                    .and_then(|v| {
                        v.get("jobs").and_then(|j| j.as_array().cloned())
                            .or_else(|| v.as_array().cloned())
                    });
                match parsed {
                    Some(jobs) if jobs.is_empty() => "No cron jobs configured.".to_owned(),
                    Some(jobs) => {
                        let mut lines = vec!["Cron jobs:".to_owned()];
                        for job in &jobs {
                            let id = job.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                            let schedule = job.get("schedule").and_then(|v| v.as_str()).unwrap_or("?");
                            let agent = job.get("agentId").and_then(|v| v.as_str()).unwrap_or("main");
                            let msg = job.get("message").and_then(|v| v.as_str()).unwrap_or("");
                            let enabled = job.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                            let status = if enabled { "" } else { " (disabled)" };
                            let msg_preview = if msg.len() > 50 {
                                let end = msg.char_indices().nth(47).map(|(i, _)| i).unwrap_or(msg.len());
                                format!("{}...", &msg[..end])
                            } else {
                                msg.to_owned()
                            };
                            lines.push(format!("  [{}] {} -> {} \"{}\"{}",
                                id, schedule, agent, msg_preview, status));
                        }
                        lines.join("\n")
                    }
                    None => "No cron jobs configured.".to_owned(),
                }
            }
            Err(_) => "No cron jobs configured.".to_owned(),
        };
        return Some(txt(reply));
    }
    // /model — show current model; /models — list providers; /model <name> — switch
    if lower == "/model" || lower == "/models" {
        let model = handle.config.model.as_ref()
            .and_then(|m| m.primary.as_deref())
            .unwrap_or("default");
        let mut lines = vec![format!("Current model: {model}")];
        lines.push(String::new());
        lines.push("Registered providers:".to_owned());
        for name in handle.providers.names() {
            lines.push(format!("  {name}"));
        }
        return Some(txt(lines.join("\n")));
    }
    if lower.starts_with("/model ") {
        let model = t.get(7..).unwrap_or("").trim();
        return Some(txt(format!("Model switched to: {model} (runtime only, use configure to persist)")));
    }
    // /run <cmd>, /sh <cmd>, /exec <cmd>, ! <cmd>, $ <cmd> — shell execution
    let shell_cmd: Option<&str> = if lower.starts_with("/run ")
        || lower.starts_with("/sh ")
        || lower.starts_with("/exec ")
    {
        t.find(' ').map(|i| t[i + 1..].trim())
    } else if t.starts_with("! ") {
        Some(t[2..].trim())
    } else if t.starts_with("$ ") {
        Some(t[2..].trim())
    } else {
        None
    };
    if let Some(cmd) = shell_cmd {
        let (shell, arg) = if cfg!(target_os = "windows") {
            ("powershell", "-Command")
        } else {
            ("sh", "-c")
        };
        let ws = workspace();
        let mut proc = tokio::process::Command::new(shell);
        proc.args([arg, cmd])
            .current_dir(&ws)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            proc.creation_flags(CREATE_NO_WINDOW);
        }
        let out = proc.output().await;
        let reply = match out {
            Ok(o) => {
                let mut result = String::from_utf8_lossy(&o.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&o.stderr);
                if !stderr.trim().is_empty() {
                    if !result.is_empty() { result.push('\n'); }
                    result.push_str(stderr.trim());
                }
                if result.trim().is_empty() {
                    if o.status.success() { "(no output)".to_owned() }
                    else { format!("exit {}", o.status.code().unwrap_or(-1)) }
                } else { result }
            }
            Err(e) => format!("exec error: {e}"),
        };
        return Some(txt(reply));
    }
    None
}

/// Check if a message is a fast preparse command that should bypass the per-user queue.
/// These are local slash commands that execute instantly and should not wait behind
/// slow LLM requests in the queue.
pub(crate) fn is_fast_preparse(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_lowercase();
    // Single-word commands (no args needed)
    matches!(
        lower.as_str(),
        "/ls" | "/status" | "/version" | "/help" | "/?" | "/health" | "/uptime"
            | "/model" | "/models" | "/cron" | "/clear" | "/new" | "/reset" | "/abort" | "/sessions"
    )
    // Commands with optional/required args
    || lower.starts_with("/ls ")
    || lower.starts_with("/cat ")
    || lower.starts_with("/ss")
    || lower.starts_with("/remember ")
    || lower.starts_with("/recall ")
    || lower.starts_with("/cron ")
    || lower.starts_with("/skill ")
    || lower.starts_with("/model ")
    || lower.starts_with("/run ")
    || lower.starts_with("/sh ")
    || lower.starts_with("/exec ")
    || t.starts_with("! ")
    || t.starts_with("$ ")
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Processing indicator — send with 3s timeout to avoid blocking
// ---------------------------------------------------------------------------

/// Returns the configured processing timeout duration (default 120s).
/// When set to 0, returns a very large duration (effectively disabled).
/// When intermediateOutput is enabled, also disabled (intermediate text replaces this).
pub(crate) fn processing_timeout(config: &RuntimeConfig) -> Duration {
    let intermediate = config.raw.agents.as_ref()
        .and_then(|a| a.defaults.as_ref())
        .and_then(|d| d.intermediate_output)
        .unwrap_or(true);
    if intermediate {
        return Duration::from_secs(86400);
    }
    let secs = config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.processing_timeout)
        .unwrap_or(120);
    if secs == 0 {
        Duration::from_secs(86400)
    } else {
        Duration::from_secs(secs)
    }
}

pub(crate) async fn send_processing(
    tx: &mpsc::Sender<OutboundMessage>,
    target_id: String,
    is_group: bool,
    config: &RuntimeConfig,
) {
    let i18n_lang = config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.language.as_deref())
        .map(crate::i18n::resolve_lang)
        .unwrap_or("en");
    let text = crate::i18n::t("processing", i18n_lang);
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        tx.send(OutboundMessage {
            target_id,
            is_group,
            text,
            reply_to: None,
            images: vec![],
            channel: None,

                    files: vec![],        }),
    )
    .await;
}

// ---------------------------------------------------------------------------
// /btw direct LLM call — bypasses agent inbox entirely
// ---------------------------------------------------------------------------

/// Perform a direct LLM call for /btw side queries, bypassing the agent inbox.
/// Reads the agent's live status so the LLM knows what the main agent is doing.
/// Returns the response text, or None on failure.
pub(crate) async fn btw_direct_call(
    question: &str,
    live_status: &tokio::sync::RwLock<LiveStatus>,
    providers: &ProviderRegistry,
    config: &RuntimeConfig,
) -> Option<String> {
    // 1. Read live status.
    let status_block = {
        let status = live_status.read().await;
        if status.state.is_empty() || status.state == "idle" {
            String::new()
        } else {
            let elapsed = status
                .started_at
                .map(|s| s.elapsed().as_secs())
                .unwrap_or(0);
            format!(
                "\n<main_agent_status>\nState: {}\nTask: {}\nElapsed: {}s\nRecent tools: {}\nResponse preview: {}\n</main_agent_status>",
                status.state,
                status.current_task,
                elapsed,
                status.tool_history.join(", "),
                status.text_preview,
            )
        }
    };

    // 2. Resolve model name.
    let model = config
        .agents
        .defaults
        .model
        .as_ref()
        .and_then(|m| m.primary.as_deref())
        .unwrap_or("anthropic/claude-sonnet-4-6");

    let system = format!(
        "You are answering a quick side question (/btw). Be concise and direct. \
         You have no tools available. Answer from your general knowledge only.{}",
        status_block
    );

    let req = LlmRequest {
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(question.to_owned()),
        }],
        tools: vec![],
        system: Some(system),
        max_tokens: Some(500),
        temperature: None,
        frequency_penalty: None,
        thinking_budget: None,
    };

    // 3. Create a simple failover manager (no fallbacks needed for /btw).
    let auth_order = config
        .model
        .auth
        .as_ref()
        .and_then(|a| a.order.clone())
        .unwrap_or_default();
    let mut failover = FailoverManager::new(auth_order, std::collections::HashMap::new(), vec![]);

    let mut stream = match failover.call(req, providers).await {
        Ok(s) => s,
        Err(e) => {
            warn!("/btw direct LLM call failed: {e:#}");
            return None;
        }
    };

    let mut text_buf = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(d)) => text_buf.push_str(&d),
            Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
            Ok(_) => {}
            Err(e) => {
                warn!("/btw stream error: {e:#}");
                break;
            }
        }
    }

    if text_buf.is_empty() {
        None
    } else {
        // Strip any residual <think>...</think> tags
        let cleaned = crate::provider::openai::strip_think_tags_pub(&text_buf);
        Some(cleaned)
    }
}
