//! System prompt builders — base prompt, full prompt, help text, and helpers.
//!
//! Extracted from `runtime.rs` to reduce file size.

use super::workspace::WorkspaceContext;
use crate::skill::SkillRegistry;

/// Read-only commands that are always allowed for any agent (regardless of
/// allowedCommands).
pub(crate) const READONLY_COMMANDS: &[&str] = &[
    "/help", "/version", "/status", "/health", "/uptime", "/models", "/ctx", "/btw", "/clear",
    "/compact", "/history", "/cron", "/abort",
];

/// Format a Duration as human-readable string.
pub(crate) fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m {s}s")
    } else if mins > 0 {
        format!("{mins}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Build filtered help text based on allowed commands and language.
pub(crate) fn build_help_text_filtered(allowed: &str, lang: &str) -> String {
    let full = allowed == "*";
    let zh = lang == "zh";
    let has = |cmd: &str| -> bool {
        if full { return true; }
        READONLY_COMMANDS.iter().any(|c| *c == cmd) || allowed.split('|').any(|a| a.trim() == cmd)
    };

    let mut h = String::from(if zh { "可用命令：\n\n" } else { "Available commands:\n\n" });

    if has("/run") || has("/find") || has("/grep") {
        h.push_str(if zh { "终端：\n" } else { "Shell:\n" });
        if has("/run") {
            h.push_str(if zh { "  /run <命令>       执行终端命令\n  $ <命令>           执行终端命令（快捷方式）\n" } else { "  /run <cmd>        Execute a shell command\n  $ <cmd>           Execute a shell command (shortcut)\n" });
        }
        if has("/find") { h.push_str(if zh { "  /find <模式>      按名称查找文件\n" } else { "  /find <pattern>   Find files by name\n" }); }
        if has("/grep") { h.push_str(if zh { "  /grep <模式>      搜索文件内容\n" } else { "  /grep <pattern>   Search file contents\n" }); }
        h.push('\n');
    }

    if has("/read") || has("/write") || has("/ls") {
        h.push_str(if zh { "文件：\n" } else { "Files:\n" });
        if has("/read") { h.push_str(if zh { "  /read <路径>      读取文件\n" } else { "  /read <path>      Read a file\n" }); }
        if has("/write") { h.push_str(if zh { "  /write <路径> <内容>  写入文件\n" } else { "  /write <path> <content>  Write to a file\n" }); }
        if has("/ls") { h.push_str(if zh { "  /ls [路径]        列出目录\n" } else { "  /ls [path]        List directory\n" }); }
        h.push('\n');
    }

    if has("/search") || has("/fetch") || has("/screenshot") || has("/ss") {
        h.push_str(if zh { "搜索与网页：\n" } else { "Search & Web:\n" });
        if has("/search") { h.push_str(if zh { "  /search <关键词>  搜索网页\n" } else { "  /search <query>   Search the web\n" }); }
        if has("/fetch") { h.push_str(if zh { "  /fetch <网址>     抓取网页内容\n" } else { "  /fetch <url>      Fetch a web page\n" }); }
        if has("/screenshot") { h.push_str(if zh { "  /screenshot <网址> 网页截图\n" } else { "  /screenshot <url> Screenshot a web page\n" }); }
        if has("/ss") { h.push_str(if zh { "  /ss               桌面截图\n" } else { "  /ss               Screenshot desktop\n" }); }
        h.push('\n');
    }

    if has("/remember") || has("/recall") {
        h.push_str(if zh { "记忆：\n" } else { "Memory:\n" });
        if has("/remember") { h.push_str(if zh { "  /remember <文本>  保存到记忆\n" } else { "  /remember <text>  Save to memory\n" }); }
        if has("/recall") { h.push_str(if zh { "  /recall <关键词>  搜索记忆\n" } else { "  /recall <query>   Search memory\n" }); }
        h.push('\n');
    }

    h.push_str(if zh { "背景上下文：\n" } else { "Background Context:\n" });
    h.push_str(if zh { "  /ctx <文本>              添加持久上下文\n" } else { "  /ctx <text>              Add persistent context\n" });
    h.push_str(if zh { "  /ctx --ttl <N> <文本>    添加上下文（N轮后过期）\n" } else { "  /ctx --ttl <N> <text>    Add context (expires in N turns)\n" });
    if full { h.push_str(if zh { "  /ctx --global <文本>     添加全局上下文\n" } else { "  /ctx --global <text>     Add global context (all sessions)\n" }); }
    h.push_str(if zh { "  /ctx --list              列出活跃上下文\n" } else { "  /ctx --list              List active context entries\n" });
    h.push_str(if zh { "  /ctx --remove <id>       移除指定上下文\n" } else { "  /ctx --remove <id>       Remove entry by id\n" });
    h.push_str(if zh { "  /ctx --clear             清除当前会话所有上下文\n" } else { "  /ctx --clear             Clear all context for this session\n" });
    h.push('\n');

    h.push_str(if zh { "快速提问：\n" } else { "Side Query:\n" });
    h.push_str(if zh { "  /btw <问题>              快速查询（不调用工具）\n" } else { "  /btw <question>          Quick query (no tools, ephemeral)\n" });
    h.push('\n');

    if full {
        h.push_str(if zh { "工具（聚合）：\n" } else { "Tools (consolidated):\n" });
        h.push_str(if zh { "  memory   搜索/获取/保存/删除长期记忆\n" } else { "  memory   search/get/put/delete long-term memory\n" });
        h.push_str(if zh { "  session  发送/列表/历史/状态\n" } else { "  session  send/list/history/status for sessions\n" });
        h.push_str(if zh { "  agent    创建/任务/列表/终止子智能体\n" } else { "  agent    spawn/task/list/kill sub-agents\n" });
        h.push_str(if zh { "  channel  发送/回复/置顶/删除跨渠道消息\n" } else { "  channel  send/reply/pin/delete across channels\n" });
        h.push('\n');
    }

    h.push_str(if zh { "系统：\n" } else { "System:\n" });
    h.push_str(if zh { "  /status           网关状态\n" } else { "  /status           Gateway status\n" });
    h.push_str(if zh { "  /version          查看版本\n" } else { "  /version          Show version\n" });
    h.push_str(if zh { "  /models           列出模型\n" } else { "  /models           List models\n" });
    if has("/model") { h.push_str(if zh { "  /model <名称>     切换模型\n" } else { "  /model <name>     Switch model\n" }); }
    h.push_str(if zh { "  /uptime           查看运行时长\n" } else { "  /uptime           Show uptime\n" });
    h.push('\n');

    h.push_str(if zh { "会话：\n" } else { "Session:\n" });
    h.push_str(if zh { "  /clear            清除会话\n" } else { "  /clear            Clear session\n" });
    h.push_str(if zh { "  /compact          压缩会话并保存记忆\n" } else { "  /compact          Compact session & save to memory\n" });
    h.push_str(if zh { "  /abort            终止当前任务\n" } else { "  /abort            Abort running task\n" });
    if has("/reset") { h.push_str(if zh { "  /reset            重置会话\n" } else { "  /reset            Reset session\n" }); }
    h.push_str(if zh { "  /voice            语音回复模式\n" } else { "  /voice            Voice reply mode\n" });
    h.push_str(if zh { "  /text             文字回复模式\n" } else { "  /text             Text reply mode\n" });
    h.push_str(if zh { "  /history [n]      查看历史\n" } else { "  /history [n]      Show history\n" });
    if has("/sessions") { h.push_str(if zh { "  /sessions         列出会话\n" } else { "  /sessions         List sessions\n" }); }
    h.push('\n');

    h.push_str(if zh { "定时任务：\n" } else { "Cron:\n" });
    h.push_str(if zh { "  /cron list        列出定时任务\n" } else { "  /cron list        List cron jobs\n" });
    h.push('\n');

    if has("/send") {
        h.push_str(if zh { "消息：\n" } else { "Messaging:\n" });
        h.push_str(if zh { "  /send <目标> <消息>  发送消息\n" } else { "  /send <target> <msg>  Send a message\n" });
        h.push('\n');
    }

    if has("/skill") {
        h.push_str(if zh { "技能：\n" } else { "Skill:\n" });
        h.push_str("  /skill install <name>\n  /skill list\n  /skill search <query>\n");
        h.push('\n');
    }

    if full {
        h.push_str(if zh { "上传限制：\n" } else { "Upload & Limits:\n" });
        h.push_str(if zh {
            "  /get_upload_size           查看上传大小限制\n  /set_upload_size <MB>      设置大小限制\n  /get_upload_chars          查看文本字符限制\n  /set_upload_chars <N>      设置字符限制\n  /config_upload_size <MB>   持久化大小限制\n  /config_upload_chars <N>   持久化字符限制\n"
        } else {
            "  /get_upload_size           Show upload size limit\n  /set_upload_size <MB>      Set size limit (runtime)\n  /get_upload_chars          Show text char limit\n  /set_upload_chars <N>      Set char limit (runtime)\n  /config_upload_size <MB>   Set size limit (persistent)\n  /config_upload_chars <N>   Set char limit (persistent)\n"
        });
        h.push('\n');
    }

    h.push_str(if zh { "直接输入消息即可与AI对话。" } else { "Type any message without / to chat with the AI agent." });
    h
}

// ---------------------------------------------------------------------------
// System prompt builder
// ---------------------------------------------------------------------------

/// Build the base system prompt shared by main agent and sub agents.
///
/// Contains: date/time, language, platform, command safety rules.
/// Sub agents call this directly; the main agent calls `build_system_prompt`
/// which adds workspace context, skills, and tool guidance on top.
pub(crate) fn build_base_system_prompt(config: &crate::config::schema::Config) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();

    // Current date/time so the model knows "today", "last Friday", etc.
    let now = chrono::Local::now();
    use chrono::Datelike;
    let weekday = now.date_naive().weekday().num_days_from_monday();
    let last_friday = if weekday >= 4 {
        now.date_naive() - chrono::Duration::days((weekday - 4) as i64)
    } else {
        now.date_naive() - chrono::Duration::days((weekday + 3) as i64)
    };
    let yesterday = now.date_naive() - chrono::Duration::days(1);
    let mut date_line = format!(
        "Current date: {} ({}). Yesterday: {}. Last Friday: {}.",
        now.format("%Y-%m-%d %H:%M"),
        now.format("%A"),
        yesterday.format("%Y-%m-%d"),
        last_friday.format("%Y-%m-%d"),
    );
    if let Some(lang) = config.gateway.as_ref().and_then(|g| g.language.as_deref()) {
        date_line.push_str(&format!(
            "\nDefault response language: {lang}. Always reply in {lang} unless the user explicitly uses another language."
        ));
    }
    parts.push(date_line);

    // Platform information so LLM generates correct shell commands.
    let platform_info = if cfg!(target_os = "windows") {
        "Platform: Windows. Shell: PowerShell. \
         Use PowerShell commands: Get-ChildItem (or dir), Get-Content, Get-Date, Select-Object -Last N (tail). \
         Pipes and filters work naturally: | Where-Object, | Select-Object, | Sort-Object. \
         Paths: backslash or forward slash both work. \
         Examples: Get-Date -Format 'yyyy-MM-dd'; Get-ChildItem | Select-Object -Last 5; Get-Content file.txt."
    } else if cfg!(target_os = "macos") {
        "Platform: macOS. Shell: bash/zsh. Standard Unix commands available (ls, cat, grep, tail, date)."
    } else {
        "Platform: Linux. Shell: bash/sh. Standard Unix commands available (ls, cat, grep, tail, date)."
    };
    parts.push(platform_info.to_string());

    // Windows command safety rules (only on Windows builds).
    if cfg!(target_os = "windows") {
        parts.push(
            "<windows_command_safety>\n\
             Windows command safety rules (ALL mandatory):\n\
             1. Do not wrap a command in an extra shell layer such as `cmd /c`, `powershell -Command`, or `pwsh -Command` unless strictly necessary.\n\
             2. For destructive file operations, only use a fully specified absolute path.\n\
             3. Never generate a command whose quoting, escaping, or trailing backslashes could cause the target path to be truncated or reinterpreted.\n\
             4. Any destructive operation outside the workspace requires explicit user approval.\n\
             5. If a destructive command fails, do NOT retry with workarounds or alternate commands. Stop, explain the failure, and ask the user.\n\
             </windows_command_safety>"
                .to_owned(),
        );
    }

    // Agent loop guidance (helps small models understand the iteration pattern).
    parts.push(
        "<agent_loop>\n\
         You are operating in an agent loop:\n\
         1. Analyze: understand the user's intent and current state\n\
         2. Plan: decide which tool to use next\n\
         3. Execute: call the tool\n\
         4. Observe: check the result\n\
         5. Iterate: repeat until the task is complete, then reply to the user\n\
         If a tool call fails, do NOT retry with the same arguments. Try a different approach or inform the user.\n\
         Never fabricate URLs, file paths, or numeric values.\n\
         When you need a Unix timestamp, use a shell command (e.g. `date +%s`) — never calculate it yourself.\n\
         </agent_loop>"
            .to_owned(),
    );

    parts
}

/// Build the full system prompt for the main agent (base + workspace + skills + tools).
pub(crate) fn build_system_prompt(
    ws_ctx: &WorkspaceContext,
    skills: &SkillRegistry,
    config: &crate::config::schema::Config,
) -> String {
    let mut parts = build_base_system_prompt(config);

    // Tool usage guidance
    {
        parts.push(
            "## Tool Usage Guidelines\n\
             ### File Operations (use dedicated tools, NOT execute_command)\n\
             - List directory contents: use `list_dir` (NOT execute_command ls/dir)\n\
             - Find files by name: use `search_file` (NOT execute_command find)\n\
             - Search file contents: use `search_content` (NOT execute_command grep)\n\
             - Read file: use `read_file`. Write/create file: use `write_file`.\n\
             - For documents (xlsx/docx/pdf/pptx): use the `doc` tool, not execute_command.\n\
             - Reserve `execute_command` for system commands and tasks that have no dedicated tool.\n\
             ### Completion Discipline (CRITICAL)\n\
             - When you have enough information to answer the user, STOP and reply immediately.\n\
             - Do NOT search for additional confirmation after finding the answer.\n\
             - Do NOT repeat a tool call that already returned useful results.\n\
             - One successful search/fetch is usually enough. Two is the maximum for verification.\n\
             - If a web_browser operation produced the content the user asked for, deliver it — do not navigate further.\n\
             ### Web Operations\n\
             - When user asks to go to a specific site (e.g. 'go to douyin', 'open taobao'), use `web_browser` directly. Do NOT search first.\n\
             - For general questions or info lookup, use `web_search` first.\n\
             - To download files/images/videos: use `web_download` (supports resume, browser cookies). Do NOT use exec curl/wget.\n\
             - `web_download` path is relative to workspace/downloads/. Just pass the filename like `video.mp4` or `subdir/file.pdf`. Do NOT include `~/`, `~/Downloads/`, or absolute paths.\n\
             - After downloading, use `send_file` to send the file to the user.\n\
             ### Agent & Task Delegation\n\
             You are the architect. Delegate work to sub-agents, never block.\n\
             - Use `agent` action=task for one-shot sub-tasks. Tasks run in the background and results appear on your next turn.\n\
             - Always specify a `toolset` matching the task (web=search/browse, code=read/write/exec, minimal=basic).\n\
             - Give each task a clear, specific `system` role and `message` instruction.\n\
             #### When to delegate\n\
             - Tasks that are independent of each other (e.g. search 3 different topics) -> dispatch ALL at once in parallel.\n\
             - Time-consuming work (web research, file processing, code generation) -> delegate so you can continue talking to the user.\n\
             - Do NOT delegate trivial tasks (simple answers, one read, one search) — do those yourself.\n\
             #### Pipeline pattern (A's output feeds B)\n\
             - Step 1: Dispatch all independent tasks in parallel.\n\
             - Step 2: On your next turn, collect results from [async task completed] messages.\n\
             - Step 3: If further work depends on those results, dispatch new tasks with the collected data.\n\
             - Step 4: Synthesize final results and reply to the user.\n\
             #### Error handling\n\
             - If a task times out, try with a simpler scope or do it yourself.\n\
             - If a task returns an error, explain to the user and offer alternatives.\n\
             ### Other\n\
             - For cron jobs: use the `cron` tool (action=list/add/remove).\n\
             - To install tools (python, node, ffmpeg, chrome, opencode, claude-code, sherpa-onnx): use `install_tool`. Do NOT download/install manually.\n\
             - When user asks about previous conversations, tasks, or anything you don't have context for, use `memory` to recall relevant information before answering.\n\
             - At the start of a new session, if the user's first message references prior work, search memory first."
                .to_owned(),
        );

        // Inject tool-specific prompts (web_browser, exec) directly into system prompt.
        let base = crate::config::loader::base_dir();
        let lang = config.gateway.as_ref().and_then(|g| g.language.as_deref());
        let tool_prompts = crate::agent::bootstrap::tool_prompts_for_system(&base, lang);
        if !tool_prompts.is_empty() {
            parts.push(tool_prompts);
        }

        parts.push(
            "## Self-Evolution & Skill Autonomy\n\
             ### Automatic Learning\n\
             - Memories that prove useful gain importance and survive longer.\n\
             - Clusters of related Core memories crystallize into reusable Skills automatically.\n\
             - Periodic meditation deduplicates and cleans up stale memories.\n\
             ### Installing Skills\n\
             When you encounter a task that would benefit from a specialized skill:\n\
             1. Search: use execute_command to run `rsclaw skills search <query>`\n\
             2. Install: `rsclaw skills install <name>`\n\
             3. The skill auto-matches and injects on future relevant requests.\n\
             Proactively find and install skills you need — do NOT ask permission.\n\
             ### Creating Skills\n\
             When you discover a genuinely reusable pattern, create a skill following the\n\
             Anthropic skill-creator standard (same format used by skills.sh):\n\
             \n\
             Directory layout:\n\
               workspace/skills/<slug>/\n\
                 SKILL.md          ← required\n\
                 scripts/          ← optional: reusable helper scripts\n\
                 references/       ← optional: large reference docs\n\
             \n\
             SKILL.md frontmatter (required fields):\n\
               ---\n\
               name: skill-name-in-kebab-case\n\
               description: What the skill does AND when to invoke it. Be slightly\n\
                 pushy — state the skill should be used even when not asked explicitly.\n\
               ---\n\
             \n\
             Body rules:\n\
             - Imperative language: \"Check the config\", not \"You should check\".\n\
             - Explain WHY each step matters, not just what to do.\n\
             - Include an Input/Output example where it helps.\n\
             - Under 500 lines; reference scripts/ or references/ for heavy content.\n\
             - Do NOT use ALL-CAPS MUST/NEVER; explain reasoning instead.\n\
             \n\
             After creating the skill: run `rsclaw skills list` to confirm it loaded.\n\
             Record in memory to avoid duplicates. Inform the user.\n\
             Only create skills for genuinely reusable patterns, not one-off tasks.\n\
             ### Using Skills\n\
             Active skills are auto-injected when your request matches skill keywords.\n\
             Follow skill instructions carefully — they encode validated experience.\n\
             If a skill's approach fails, fall back to general methods and update the skill."
                .to_owned(),
        );
    }

    // Workspace files segment.
    let ws_segment = ws_ctx.to_prompt_segment();
    if !ws_segment.is_empty() {
        parts.push(ws_segment);
    }

    // Available skills — name + short description. Full prompts injected on-demand.
    if !skills.is_empty() {
        let lines: Vec<_> = skills
            .all()
            .map(|s| {
                format!("- {}", s.name)
            })
            .collect();
        if !lines.is_empty() {
            parts.push(format!("Available skills:\n{}", lines.join("\n")));
        }
    }

    parts.join("\n\n")
}

/// Return a relative time label for memory recall.
/// LLMs can't do date arithmetic, so we use relative descriptions.
pub(crate) fn memory_age_label(now_ts: i64, created_at: i64) -> String {
    let age_secs = (now_ts - created_at).max(0);
    let days = age_secs / 86400;
    match days {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        2..=6 => format!("{days} days ago"),
        7..=13 => "~1 week ago".to_owned(),
        14..=29 => format!("{} weeks ago", days / 7),
        30..=59 => "~1 month ago — may be outdated, verify before using".to_owned(),
        60..=364 => format!("{} months ago — may be outdated, verify before using", days / 30),
        365..=729 => "~1 year ago — likely outdated, verify before using".to_owned(),
        _ => format!("~{} years ago — likely outdated, verify before using", days / 365),
    }
}
