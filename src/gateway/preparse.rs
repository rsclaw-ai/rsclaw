//! Fast preparse — local slash-command handling that bypasses the agent queue.
//!
//! Functions here handle commands like `/ls`, `/status`, `/version`, `/btw`
//! without going through the LLM agent loop.

use futures::StreamExt as _;
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
///
/// `channel` (e.g. "telegram", "wechat") and `peer_id` are passed through so commands
/// that need to schedule deliveries back to the originating channel/peer (e.g. `/loop`)
/// can populate a `CronDelivery` correctly.
pub(crate) async fn try_preparse_locally(
    text: &str,
    handle: &crate::agent::AgentHandle,
    channel: &str,
    peer_id: &str,
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
        return Some(txt(format!("rsclaw v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"))));
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
        let flags = handle.abort_flags.read().expect("abort_flags lock poisoned");
        let count = flags.len();
        for f in flags.values() { f.store(true, Ordering::SeqCst); }
        return Some(txt(if count > 0 { format!("abort signal sent ({count} session(s))") } else { "nothing to abort".to_owned() }));
    }
    // /clear — abort running turns + signal session clear (fully non-blocking)
    if lower == "/clear" {
        // 1. Abort all running turns
        let flags = handle.abort_flags.read().expect("abort_flags lock poisoned");
        for f in flags.values() { f.store(true, Ordering::SeqCst); }
        drop(flags);
        // 2. Signal runtime to clear sessions at next opportunity
        handle.clear_signal.store(true, Ordering::SeqCst);
        return Some(txt("Session cleared.".to_owned()));
    }
    // /new — start a fresh conversation (new generation, no summary)
    if lower == "/new" {
        let flags = handle.abort_flags.read().expect("abort_flags lock poisoned");
        for f in flags.values() { f.store(true, Ordering::SeqCst); }
        drop(flags);
        handle.new_session_signal.store(true, Ordering::SeqCst);
        return Some(txt("New session started.".to_owned()));
    }
    // /reset — reset current session (no summary, same generation)
    if lower == "/reset" {
        let flags = handle.abort_flags.read().expect("abort_flags lock poisoned");
        for f in flags.values() { f.store(true, Ordering::SeqCst); }
        drop(flags);
        handle.reset_signal.store(true, Ordering::SeqCst);
        return Some(txt("Session reset.".to_owned()));
    }
    // /status
    if lower == "/status" {
        return Some(txt(handle.format_status()));
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
    // /cron — list cron jobs (reads from disk).  Routes through the same
    // formatter the agent runtime uses for `__CRON_LIST__` so feishu/wechat/etc.
    // see the same output format as the desktop console.
    if lower == "/cron" || lower == "/cron list" {
        let jobs_path = crate::config::loader::base_dir().join("cron.json5");
        let jobs = crate::agent::tools_cron::read_cron_jobs(&jobs_path).await;
        let reply = crate::agent::tools_cron::format_cron_jobs(&jobs);
        return Some(txt(reply));
    }
    // /loop <interval> <prompt-or-cmd> — schedule a recurring agentTurn
    // back to the originating channel/peer. Persists to cron.json5 and
    // signals the cron runner to reload.
    if lower == "/loop" || lower == "/loop -h" || lower == "/loop --help" || lower == "/loop help" {
        return Some(txt(loop_help_text(crate::i18n::default_lang())));
    }
    if lower.starts_with("/loop ") {
        let rest = t.get(6..).unwrap_or("").trim();
        let (interval_s, prompt) = match rest.split_once(char::is_whitespace) {
            Some((iv, pr)) => (iv.trim(), pr.trim()),
            None => (rest, ""),
        };
        if interval_s.is_empty() || prompt.is_empty() {
            return Some(txt(loop_help_text(crate::i18n::default_lang())));
        }
        let every_ms = match parse_interval_ms(interval_s) {
            Some(v) if v >= 2_000 => v,
            Some(_) => return Some(txt("/loop: interval must be >= 2s".to_owned())),
            None => return Some(txt(format!("/loop: cannot parse interval `{interval_s}` (e.g. 30s, 5m, 1h, 2h30m)"))),
        };
        if peer_id.is_empty() || channel.is_empty() {
            return Some(txt("/loop: missing channel/peer context (cannot schedule delivery)".to_owned()));
        }
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        let id = format!("loop-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        // ws transport is registered as "desktop" on the delivery side.
        let delivery_channel: &str = if channel == "ws" { "desktop" } else { channel };
        let job = serde_json::json!({
            "id": id,
            "agentId": handle.id,
            "enabled": true,
            "schedule": {"kind": "every", "everyMs": every_ms, "anchorMs": now_ms},
            "payload": {"kind": "agentTurn", "text": prompt},
            "delivery": {"channel": delivery_channel, "to": peer_id, "mode": "always"},
            "createdAtMs": now_ms,
        });
        let cron_path = crate::cron::resolve_cron_store_path();
        let _guard = crate::cron::CRON_FILE_LOCK.lock().await;
        let mut jobs = crate::agent::tools_cron::read_cron_jobs(&cron_path).await;
        jobs.push(job);
        if let Err(e) = crate::agent::tools_cron::write_cron_jobs(&cron_path, &jobs).await {
            return Some(txt(format!("/loop: failed to save jobs: {e}")));
        }
        drop(_guard);
        crate::cron::trigger_reload();
        let zh = crate::i18n::default_lang() == "zh";
        let human = format_interval_ms(every_ms);
        return Some(txt(if zh {
            format!("已安排循环（每 {human}）：{prompt}\nID: {id}\n停止：/cron remove {id}（通过 agent）")
        } else {
            format!("Scheduled loop (every {human}): {prompt}\nID: {id}\nStop with: /cron remove {id} (via agent)")
        }));
    }
    // /task with no args or -h/--help → print task help (short-circuit;
    // otherwise it would route into the task queue with an empty message).
    if lower == "/task" || lower == "/task -h" || lower == "/task --help" || lower == "/task help" {
        return Some(txt(task_help_text(crate::i18n::default_lang())));
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
        tracing::warn!(command = %cmd, "executing shell command via preparse (open dmPolicy)");

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
            | "/loop" | "/task"
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
    || lower.starts_with("/loop ")
    // /task only short-circuits on help variants; non-help forms must NOT
    // bypass the queue (the task queue worker owns the multi-turn flow).
    || lower == "/task -h"
    || lower == "/task --help"
    || lower == "/task help"
    || t.starts_with("! ")
    || t.starts_with("$ ")
}

// ---------------------------------------------------------------------------
// Helpers for /loop
// ---------------------------------------------------------------------------

/// Parse a human-readable interval like "30s", "5m", "1h", "2h30m", "1d".
/// A bare number is interpreted as seconds.
fn parse_interval_ms(s: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut num_buf = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num_buf.push(c);
        } else if c.is_ascii_whitespace() {
            continue;
        } else if c.is_ascii_alphabetic() {
            if num_buf.is_empty() {
                return None;
            }
            let n: u64 = num_buf.parse().ok()?;
            num_buf.clear();
            let mul: u64 = match c.to_ascii_lowercase() {
                's' => 1_000,
                'm' => 60_000,
                'h' => 3_600_000,
                'd' => 86_400_000,
                _ => return None,
            };
            total = total.checked_add(n.checked_mul(mul)?)?;
        } else {
            return None;
        }
    }
    if !num_buf.is_empty() {
        // Bare trailing number → seconds (e.g. "300" → 300s).
        let n: u64 = num_buf.parse().ok()?;
        total = total.checked_add(n.checked_mul(1_000)?)?;
    }
    if total == 0 { None } else { Some(total) }
}

/// Format an interval in milliseconds as a compact human-readable string.
fn format_interval_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let mut parts: Vec<String> = Vec::new();
    if d > 0 { parts.push(format!("{d}d")); }
    if h > 0 { parts.push(format!("{h}h")); }
    if m > 0 { parts.push(format!("{m}m")); }
    if s > 0 { parts.push(format!("{s}s")); }
    if parts.is_empty() { "0s".to_owned() } else { parts.join("") }
}

/// Help text for /loop. Localized en/zh.
fn loop_help_text(lang: &str) -> String {
    if lang == "zh" {
        "/loop <间隔> <提示词或命令>\n\n\
         以指定间隔重复执行：把 <提示词> 当作一个新的消息发送给当前 agent，\n\
         agent 的回复会通过当前渠道返回给你。\n\
         <提示词> 可以是任何 /help 列出的命令，也可以是普通的提示词。\n\n\
         间隔示例：30s, 5m, 1h, 2h30m, 1d（最小 2s）\n\
         例：\n\
         \u{0020}\u{0020}/loop 5m 检查邮箱有没有新邮件\n\
         \u{0020}\u{0020}/loop 1h /status\n\n\
         查看：/cron list   停止：让 agent 调用 /cron remove <id>"
            .to_owned()
    } else {
        "/loop <interval> <prompt-or-command>\n\n\
         Repeat at the given interval: <prompt> is sent as a fresh message to the\n\
         current agent and its reply is delivered back through this channel.\n\
         <prompt> can be any command from /help or plain natural-language text.\n\n\
         Interval examples: 30s, 5m, 1h, 2h30m, 1d (min 2s)\n\
         Examples:\n\
         \u{0020}\u{0020}/loop 5m check for new mail\n\
         \u{0020}\u{0020}/loop 1h /status\n\n\
         List: /cron list    Stop: ask the agent to /cron remove <id>"
            .to_owned()
    }
}

/// Help text for /task. Localized en/zh.
fn task_help_text(lang: &str) -> String {
    if lang == "zh" {
        "/task [选项] <任务描述>\n\n\
         在多轮模式下执行一项任务：agent 会反复推理、调用工具，直到任务完成或耗尽预算。\n\n\
         选项：\n\
         \u{0020}\u{0020}--turns <N>      最大轮数（默认 10）\n\
         \u{0020}\u{0020}--timeout <时长> 超时（如 4h、30m，默认 1h）\n\n\
         例：\n\
         \u{0020}\u{0020}/task 修复登录页的 bug\n\
         \u{0020}\u{0020}/task --turns 20 重构支付模块\n\
         \u{0020}\u{0020}/task --timeout 4h --turns 50 完整跑通新功能\n\n\
         查看进度：/status   终止：/abort   配合 /loop 定时触发：/loop 1h /task ..."
            .to_owned()
    } else {
        "/task [options] <description>\n\n\
         Run a task in multi-turn mode: the agent will reason and call tools repeatedly\n\
         until the task is complete or its budget is exhausted.\n\n\
         Options:\n\
         \u{0020}\u{0020}--turns <N>        Max turns (default 10)\n\
         \u{0020}\u{0020}--timeout <dur>    Timeout (e.g. 4h, 30m, default 1h)\n\n\
         Examples:\n\
         \u{0020}\u{0020}/task fix the login bug\n\
         \u{0020}\u{0020}/task --turns 20 refactor the payment module\n\
         \u{0020}\u{0020}/task --timeout 4h --turns 50 finish the new feature end-to-end\n\n\
         Progress: /status   Abort: /abort   Combine with /loop: /loop 1h /task ..."
            .to_owned()
    }
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
        thinking_budget: None, kv_cache_mode: 0, session_key: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_interval_ms_basic_units() {
        assert_eq!(parse_interval_ms("30s"), Some(30_000));
        assert_eq!(parse_interval_ms("5m"), Some(300_000));
        assert_eq!(parse_interval_ms("1h"), Some(3_600_000));
        assert_eq!(parse_interval_ms("1d"), Some(86_400_000));
    }

    #[test]
    fn parse_interval_ms_compound() {
        assert_eq!(parse_interval_ms("2h30m"), Some(9_000_000));
        assert_eq!(parse_interval_ms("1h30m15s"), Some(5_415_000));
        assert_eq!(parse_interval_ms("1d2h"), Some(93_600_000));
    }

    #[test]
    fn parse_interval_ms_bare_number_is_seconds() {
        assert_eq!(parse_interval_ms("300"), Some(300_000));
    }

    #[test]
    fn parse_interval_ms_case_insensitive() {
        assert_eq!(parse_interval_ms("5M"), Some(300_000));
        assert_eq!(parse_interval_ms("1H30M"), Some(5_400_000));
    }

    #[test]
    fn parse_interval_ms_rejects_garbage() {
        assert_eq!(parse_interval_ms(""), None);
        assert_eq!(parse_interval_ms("m"), None);
        assert_eq!(parse_interval_ms("5x"), None);
        assert_eq!(parse_interval_ms("abc"), None);
    }

    #[test]
    fn format_interval_ms_drops_zero_components() {
        assert_eq!(format_interval_ms(300_000), "5m");
        assert_eq!(format_interval_ms(9_000_000), "2h30m");
        assert_eq!(format_interval_ms(86_400_000), "1d");
        assert_eq!(format_interval_ms(0), "0s");
    }

    #[test]
    fn is_fast_preparse_recognizes_loop_and_task_help() {
        assert!(is_fast_preparse("/loop"));
        assert!(is_fast_preparse("/loop 5m foo"));
        assert!(is_fast_preparse("/task"));
        assert!(is_fast_preparse("/task -h"));
        assert!(is_fast_preparse("/task --help"));
        // Real /task usage must NOT bypass the queue — task_queue owns that flow.
        assert!(!is_fast_preparse("/task fix the bug"));
        assert!(!is_fast_preparse("/task --turns 20 do something"));
    }
}
