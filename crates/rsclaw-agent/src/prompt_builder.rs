//! System prompt builders — base prompt, full prompt, help text, and helpers.
//!
//! Extracted from `runtime.rs` to reduce file size.

use std::sync::LazyLock;

use super::workspace::WorkspaceContext;
use rsclaw_plugin::{PluginRegistry, wasm_runtime::WasmPlugin};
use rsclaw_skill::SkillRegistry;

/// Cached output of [`build_shared_system_prefix`]. The shared prefix is
/// byte-stable across the gateway lifetime (no dynamic inputs — only
/// hardcoded literals + compile-time tool guidance constants), so the
/// rsclaw kvCacheMode=2 hot path was rebuilding ~3KB of identical
/// string per LLM iteration. Memoising keeps the first-call cost the
/// same and turns subsequent calls into a clone of the cached String.
static SHARED_SYSTEM_PREFIX: LazyLock<String> = LazyLock::new(build_shared_system_prefix_uncached);

/// Read-only commands that are always allowed for any agent (regardless of
/// allowedCommands).
pub(crate) const READONLY_COMMANDS: &[&str] = &[
    "/help", "/version", "/status", "/health", "/uptime", "/models", "/btw", "/clear", "/compact",
    "/history", "/cron", "/abort", "/loop", "/task", "/plugin", "/goal",
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
        if full {
            return true;
        }
        READONLY_COMMANDS.iter().any(|c| *c == cmd) || allowed.split('|').any(|a| a.trim() == cmd)
    };

    let mut h = String::from(if zh {
        "可用命令：\n\n"
    } else {
        "Available commands:\n\n"
    });

    if has("/run") || has("/find") || has("/grep") {
        h.push_str(if zh { "终端：\n" } else { "Shell:\n" });
        if has("/run") {
            h.push_str(if zh { "  /run <命令>       执行终端命令\n  $ <命令>           执行终端命令（快捷方式）\n" } else { "  /run <cmd>        Execute a shell command\n  $ <cmd>           Execute a shell command (shortcut)\n" });
        }
        if has("/find") {
            h.push_str(if zh {
                "  /find <模式>      按名称查找文件\n"
            } else {
                "  /find <pattern>   Find files by name\n"
            });
        }
        if has("/grep") {
            h.push_str(if zh {
                "  /grep <模式>      搜索文件内容\n"
            } else {
                "  /grep <pattern>   Search file contents\n"
            });
        }
        h.push('\n');
    }

    if has("/read") || has("/write") || has("/ls") {
        h.push_str(if zh { "文件：\n" } else { "Files:\n" });
        if has("/read") {
            h.push_str(if zh {
                "  /read <路径>      读取文件\n"
            } else {
                "  /read <path>      Read a file\n"
            });
        }
        if has("/write") {
            h.push_str(if zh {
                "  /write <路径> <内容>  写入文件\n"
            } else {
                "  /write <path> <content>  Write to a file\n"
            });
        }
        if has("/ls") {
            h.push_str(if zh {
                "  /ls [路径]        列出目录\n"
            } else {
                "  /ls [path]        List directory\n"
            });
        }
        h.push('\n');
    }

    if has("/search") || has("/fetch") || has("/screenshot") || has("/ss") {
        h.push_str(if zh {
            "搜索与网页：\n"
        } else {
            "Search & Web:\n"
        });
        if has("/search") {
            h.push_str(if zh {
                "  /search <关键词>  搜索网页\n"
            } else {
                "  /search <query>   Search the web\n"
            });
        }
        if has("/fetch") {
            h.push_str(if zh {
                "  /fetch <网址>     抓取网页内容\n"
            } else {
                "  /fetch <url>      Fetch a web page\n"
            });
        }
        if has("/screenshot") {
            h.push_str(if zh {
                "  /screenshot <网址> 网页截图\n"
            } else {
                "  /screenshot <url> Screenshot a web page\n"
            });
        }
        if has("/ss") {
            h.push_str(if zh {
                "  /ss               桌面截图\n"
            } else {
                "  /ss               Screenshot desktop\n"
            });
        }
        h.push('\n');
    }

    if has("/remember") || has("/recall") {
        h.push_str(if zh { "记忆：\n" } else { "Memory:\n" });
        if has("/remember") {
            h.push_str(if zh {
                "  /remember <文本>  保存到记忆\n"
            } else {
                "  /remember <text>  Save to memory\n"
            });
        }
        if has("/recall") {
            h.push_str(if zh {
                "  /recall <关键词>  搜索记忆\n"
            } else {
                "  /recall <query>   Search memory\n"
            });
        }
        h.push('\n');
    }

    h.push_str(if zh {
        "快速提问：\n"
    } else {
        "Side Query:\n"
    });
    h.push_str(if zh {
        "  /btw <问题>              快速查询（不调用工具，旁路）\n"
    } else {
        "  /btw <question>          Quick query (no tools, ephemeral, bypass)\n"
    });
    h.push('\n');

    if full {
        h.push_str(if zh {
            "工具（聚合）：\n"
        } else {
            "Tools (consolidated):\n"
        });
        h.push_str(if zh {
            "  memory   搜索/获取/保存/删除长期记忆\n"
        } else {
            "  memory   search/get/put/delete long-term memory\n"
        });
        h.push_str(if zh {
            "  session  发送/列表/历史/状态\n"
        } else {
            "  session  send/list/history/status for sessions\n"
        });
        h.push_str(if zh {
            "  agent    创建/任务/列表/终止子智能体\n"
        } else {
            "  agent    create/temp/list/kill sub-agents\n"
        });
        h.push_str(if zh {
            "  channel  发送/回复/置顶/删除跨渠道消息\n"
        } else {
            "  channel  send/reply/pin/delete across channels\n"
        });
        h.push('\n');
    }

    h.push_str(if zh { "系统：\n" } else { "System:\n" });
    h.push_str(if zh {
        "  /status           网关状态\n"
    } else {
        "  /status           Gateway status\n"
    });
    h.push_str(if zh {
        "  /version          查看版本\n"
    } else {
        "  /version          Show version\n"
    });
    h.push_str(if zh {
        "  /models           列出模型\n"
    } else {
        "  /models           List models\n"
    });
    if has("/model") {
        h.push_str(if zh {
            "  /model <名称>     切换模型\n"
        } else {
            "  /model <name>     Switch model\n"
        });
    }
    h.push_str(if zh {
        "  /uptime           查看运行时长\n"
    } else {
        "  /uptime           Show uptime\n"
    });
    h.push('\n');

    h.push_str(if zh { "会话：\n" } else { "Session:\n" });
    h.push_str(if zh {
        "  /clear            清除会话\n"
    } else {
        "  /clear            Clear session\n"
    });
    h.push_str(if zh {
        "  /compact          压缩会话并保存记忆\n"
    } else {
        "  /compact          Compact session & save to memory\n"
    });
    h.push_str(if zh {
        "  /abort            终止当前任务\n"
    } else {
        "  /abort            Abort running task\n"
    });
    h.push_str(if zh {
        "  /voice            语音回复模式\n"
    } else {
        "  /voice            Voice reply mode\n"
    });
    h.push_str(if zh {
        "  /text             文字回复模式\n"
    } else {
        "  /text             Text reply mode\n"
    });
    h.push_str(if zh {
        "  /history [n]      查看历史\n"
    } else {
        "  /history [n]      Show history\n"
    });
    if has("/sessions") {
        h.push_str(if zh {
            "  /sessions         列出会话\n"
        } else {
            "  /sessions         List sessions\n"
        });
    }
    h.push('\n');

    h.push_str(if zh { "定时任务：\n" } else { "Cron:\n" });
    h.push_str(if zh {
        "  /cron list        列出定时任务\n"
    } else {
        "  /cron list        List cron jobs\n"
    });
    h.push_str(if zh {
        "  /loop <间隔> <提示词>  循环执行（如 /loop 5m 检查邮件）\n"
    } else {
        "  /loop <interval> <prompt>  Recurring task (e.g. /loop 5m check mail)\n"
    });
    h.push('\n');

    h.push_str(if zh {
        "任务模式：\n"
    } else {
        "Task mode:\n"
    });
    h.push_str(if zh { "  /task <描述>             多轮执行任务\n  /task -n <N> -t <时长> <描述>  指定轮数和超时\n  /task -h                 查看 /task 完整帮助\n" } else { "  /task <desc>              Run a multi-turn task\n  /task -n <N> -t <dur> <desc>  Specify max turns and timeout\n  /task -h                  Full /task help\n" });
    h.push('\n');

    if has("/send") {
        h.push_str(if zh { "消息：\n" } else { "Messaging:\n" });
        h.push_str(if zh {
            "  /send <目标> <消息>  发送消息\n"
        } else {
            "  /send <target> <msg>  Send a message\n"
        });
        h.push('\n');
    }

    if has("/skill") {
        h.push_str(if zh { "技能：\n" } else { "Skill:\n" });
        h.push_str("  /skill install <name>\n  /skill list\n  /skill search <query>\n");
        h.push('\n');
    }

    if has("/plugin") {
        h.push_str(if zh { "插件：\n" } else { "Plugin:\n" });
        if zh {
            h.push_str(
                "  /plugin                       列出当前会话的插件覆盖\n  \
                 /plugin <名称>                查看某个插件的状态\n  \
                 /plugin <名称> on             启用：通过 search_tools + invoke 调用\n  \
                 /plugin <名称> all            直接注入该插件的全部工具（受总数上限）\n  \
                 /plugin <名称> <工具1,工具2>  只注入指定工具为顶层工具\n  \
                 /plugin <名称> off            完全屏蔽该插件\n  \
                 /plugin reset                 清空本会话所有覆盖\n",
            );
        } else {
            h.push_str(
                "  /plugin                         List session plugin overrides\n  \
                 /plugin <name>                  Show one plugin's current override\n  \
                 /plugin <name> on               Enable via search_tools + invoke\n  \
                 /plugin <name> all              Inject every tool from <name> (capped)\n  \
                 /plugin <name> <tool1,tool2>    Inject only the named tools as top-level\n  \
                 /plugin <name> off              Hide this plugin entirely\n  \
                 /plugin reset                   Clear every override in this session\n",
            );
        }
        h.push('\n');
    }

    if full {
        h.push_str(if zh {
            "上传限制：\n"
        } else {
            "Upload & Limits:\n"
        });
        h.push_str(if zh {
            "  /get_upload_size           查看上传大小限制\n  /set_upload_size <MB>      设置大小限制\n  /get_upload_chars          查看文本字符限制\n  /set_upload_chars <N>      设置字符限制\n  /config_upload_size <MB>   持久化大小限制\n  /config_upload_chars <N>   持久化字符限制\n"
        } else {
            "  /get_upload_size           Show upload size limit\n  /set_upload_size <MB>      Set size limit (runtime)\n  /get_upload_chars          Show text char limit\n  /set_upload_chars <N>      Set char limit (runtime)\n  /config_upload_size <MB>   Set size limit (persistent)\n  /config_upload_chars <N>   Set char limit (persistent)\n"
        });
        h.push('\n');
    }

    h.push_str(if zh {
        "直接输入消息即可与AI对话。"
    } else {
        "Type any message without / to chat with the AI agent."
    });
    h
}

// ---------------------------------------------------------------------------
// Dynamic context helper (used by KV cache modes 1 & 2)
// ---------------------------------------------------------------------------

/// Build the dynamic date/time context line.
///
/// Injected per-turn into the user message (never the system prompt) so the
/// model always sees the current wall-clock without invalidating the shared
/// kvCache=2 system-prompt prefix.
pub(crate) fn build_date_context() -> String {
    // Weekday (`%a`) is included explicitly because LLMs compute date→weekday
    // unreliably; timezone (`%Z`) disambiguates the wall-clock. Relative dates
    // (yesterday, last Friday) are dropped — the model derives those from the
    // date fine, and per-turn brevity matters.
    format!("Now: {}", chrono::Local::now().format("%Y-%m-%d %H:%M %a %Z"))
}

// ---------------------------------------------------------------------------
// System prompt builder
// ---------------------------------------------------------------------------

/// Build the **shared** system-prompt prefix.
///
/// Byte-identical for every RsClaw client of the same RsClaw version,
/// regardless of OS / language / installed skills / configured agents.
/// Designed to sit at the very front of the system prompt so an
/// upstream LLM gateway with kvCache=2 can pre-cache it once per
/// version and dedupe it across all users — the client never has to
/// resend these bytes.
///
/// Contains:
/// - `<agent_loop>` (anti-hallucination + voice rules)
/// - `[Output format rules]` + `[Data integrity rules]`
/// - `## Tool Usage Guidelines` (file ops, completion discipline, delegation,
///   GUI/desktop automation)
/// - Tool guidance prompts from `EN_TOOL_*` constants (exec, web_search,
///   web_fetch, web_browser, computer_use)
/// - `## Self-Evolution & Skill Autonomy`
///
/// Everything that varies per machine (platform info, language,
/// workspace, skills, plugins, agent persona) lives in
/// [`build_user_system`].
///
/// Backed by [`SHARED_SYSTEM_PREFIX`] (a `LazyLock<String>`) — first
/// call builds the prefix; every subsequent call clones the cached
/// String. Returning `String` rather than `&'static str` keeps the
/// existing `format!("{prefix}\n\n{suffix}")` and
/// `effective_system.strip_prefix(&shared)` callers source-compatible.
pub fn build_shared_system_prefix() -> String {
    SHARED_SYSTEM_PREFIX.clone()
}

// Pre-existing (not part of the KB work): the sequential `parts.push(..)`
// blocks below read more clearly than one giant `vec![..]` literal. Suppress
// the clippy lint locally rather than restructure unrelated prompt code.
#[allow(clippy::vec_init_then_push)]
fn build_shared_system_prefix_uncached() -> String {
    let mut parts: Vec<String> = Vec::new();

    parts.push(
        "<agent_loop>\n\
         You are operating in an agent loop:\n\
         1. Analyze: understand the user's intent and current state\n\
         2. Plan: decide which tool to use next\n\
         3. Execute: call the tool\n\
         4. Observe: check the result\n\
         5. Iterate: repeat until the task is complete, then reply to the user\n\
         If a tool call fails, do NOT retry with the same arguments — recover by error type: \
         permission denied → check the Allowed list / pick another path; timeout → set wait=false or raise the timeout; \
         rate limit → wait one turn; not found → re-read the source and retry with corrected params. \
         If the last 2 tool calls returned no NEW information relevant to the question, STOP and reply with what you have.\n\
         \n\
         [ANTI-HALLUCINATION — HARD RULES]\n\
         1. DO NOT fabricate numbers, dates, temperatures, prices, names, URLs, or any concrete facts.\n\
         2. DO NOT claim to have executed an action unless you actually made the tool call.\n\
            - If you say \"I searched\", \"I checked\", \"I delegated to X\", \"I ran Y\" — there MUST be a tool_call.\n\
            - Claiming an action without calling the tool is LYING to the user.\n\
            - If a tool is unavailable or you don't want to use it, say that honestly.\n\
         3. If a tool cannot retrieve real data (search empty, API down, access denied):\n\
            - Tell the user EXACTLY which tool failed and why.\n\
            - Ask the user if they want you to try a different approach.\n\
         4. For data that CHANGES (live scores, prices, status, balances, rankings, \"今天/现在\" anything): fetch it FRESH this turn. Never report a value from a prior fetch cached in memory/context as if it were current — a stale value passed off as current is fabrication.\n\
         Fabricating facts or pretending to have executed actions destroys user trust.\n\
         It is always better to say \"我没查到\" / \"I couldn't retrieve that\" / \"I haven't called that tool yet\"\n\
         than to invent plausible-looking but made-up values or fake action claims.\n\
         \n\
         When you need a Unix timestamp or today's date, use a shell command (e.g. `date`) — never assume or calculate it yourself.\n\
         \n\
         [Voice — HARD RULE]\n\
         Always speak directly to the user in second person (你/您/you). Never produce\n\
         third-person after-action reports about the user (e.g. \"用户东升通过...完成了...\")\n\
         or narrate what \"the user\" did. Reports, summaries, and status updates are\n\
         addressed TO the user, not ABOUT them. Do not invent completed steps — only\n\
         report what tools actually returned.\n\
         </agent_loop>"
            .to_owned(),
    );

    parts.push(
        "<context_recovery>\n\
         The runtime auto-compacts large outputs to keep context bounded — you have two recovery tools.\n\
         \n\
         **Tool-result artifact** (one-shot tool output > ~4 KB):\n\
         Tool results that crossed the size budget come back with `_truncated: true`, a head+tail\n\
         preview, and a `tool_result_id` (the inline marker `... call read_artifact(...) ...` carries it).\n\
         Full payload is on disk. Fetch with `read_artifact(tool_result_id=\"tr_xxx\", mode=...)`.\n\
         \n\
         **Session archive** (the conversation itself, after `/compact` or auto-compaction):\n\
         Older turns get summarised, but every original message is preserved in the redb archive.\n\
         When the compaction summary lacks a specific fact you need (verbatim quote, exact path, the user's\n\
         earlier wording), call `read_session_archive(mode=...)`. Modes mirror read_artifact.\n\
         \n\
         **Strategy — pick the cheapest mode that answers the question:**\n\
           - `mode=stat` first if you don't know the size — cheap, returns total_lines/byte_size.\n\
           - `mode=grep:KEYWORD` when you know a substring — scans 10000 lines in 1 response.\n\
             Alternation works: `grep:error|fail|timeout`.\n\
           - `mode=tail:N` for \"just now\" / \"刚刚\" style queries.\n\
           - `mode=head:N` for openings, prefaces, the user's original ask.\n\
           - `mode=lines:A-B` / `mode=seq:A-B` for exact ranges when you have line/seq numbers.\n\
           - `mode=full` is the most expensive — use only when you genuinely need everything.\n\
         \n\
         Every response already returns `total_lines` (read_artifact) or `total_archived`\n\
         (read_session_archive) — never call `wc -l` (Linux) or `Measure-Object` (Windows) to count;\n\
         the size is already in the JSON.\n\
         </context_recovery>"
            .to_owned(),
    );

    parts.push(
        "## CAPABILITY PRIORITY (read before every action)\n\n\
         You have THREE tool sources in THREE separate namespaces. \
         **NEVER mix them.** Confusing them is the #1 failure mode.\n\n\
         ### Namespace A — Skill\n\
         Slug like `meituan-travel`, `douyin-publish`. Discovered via \
         `skill_list(query)` or `skill_search`. Listed under \"## Installed Skills\".\n\
         **Invoke ONLY with `skill_use(name=<slug>)`** → returns the \
         SKILL.md, follow its CLI instructions via `shell`.\n\
         Never call `plugin_*` on a skill slug.\n\n\
         ### Namespace B — Plugin\n\
         Slug like `douyin`, `jimeng`, `wechat`. Discovered via `plugin_list` \
         or `plugin_search`. Listed under \"## Installed Plugins\".\n\
         **Invoke in one of two ways:**\n\
         1. **Direct ToolDef (preferred when available)** — high-frequency \
         tools are exposed in your tool list as `<plugin>__<tool>` (double \
         underscore, e.g. `douyin__publish`, `jimeng__image_txt2img`, \
         `wechat__send_text`). Call them like any other function tool. \
         The `__` IS part of the name — don't split or replace with dot.\n\
         2. **Long-tail via `plugin_invoke`** — for tools NOT in your direct \
         list: `plugin_describe(plugin, tool)` to fetch schema, then \
         `plugin_invoke(plugin, tool, arguments)` to execute.\n\
         Never call `skill_*` on a plugin slug. Never wrap a direct \
         `<plugin>__<tool>` call in `plugin_invoke`.\n\n\
         ### Namespace C — Built-in\n\
         `computer_use`, `web_search`, `web_fetch`, `web_browser`, `web_download`, \
         `shell`, `read_file`, `write_file`, `memory`, `agent`, etc. \
         **Fallback ONLY.** Use only when neither a skill nor a plugin \
         covers the user's intent.\n\n\
         ### Decision flow for every user message\n\
         1. Look at \"## Installed Plugins\" and \"## Installed Skills\" sections \
         below. Does any plugin/skill description fit the user's intent?\n\
         2. If yes → use that namespace (A or B). When BOTH a skill and \
         plugin match, prefer the plugin (more structured + WASM beats JS).\n\
         3. If no → call `skill_list(query=keyword)` AND `plugin_list` to \
         search both before falling back.\n\
         4. Built-ins (C) only after (1)–(3) come up empty.\n\n\
         ### Hard anti-patterns (do NOT do these)\n\
         - Found `meituan-travel` via `skill_list` → WRONG: called `plugin_search` for it. RIGHT: `skill_use(name=\"meituan-travel\")` — it's a skill, not a plugin.\n\
         - Found `douyin` via `plugin_list` → WRONG: called `skill_use(\"douyin\")`. RIGHT: `douyin__check_login` direct, or `plugin_invoke`. Plugins are NOT skills.\n\
         - Saw `douyin__publish` in your tool list → WRONG: wrapped it in `plugin_invoke(plugin=\"douyin\", tool=\"publish\")`. RIGHT: just call `douyin__publish(...)` directly.\n\
         - User asked about flights → WRONG: jumped to `web_fetch(ctrip.com)`. RIGHT: first `skill_list(query=\"flight\")` + `plugin_list` — a domain skill/plugin is likely there.\n\
         - Tool returned `Permission denied / Allowed:[a,b,c]` → WRONG: retried the same denied tool. RIGHT: pick one from the Allowed list and continue.\n\n\
         If a tool you tried isn't available, do NOT explain limitations to \
         the user — keep trying alternative tools or namespaces until one works."
            .to_owned(),
    );

    parts.push(
        "[Output format rules]\n\
         - Avoid Markdown headings (#, ##, ###) in chat replies.\n\
         - Use **bold text** or section markers for sections.\n\
         - Use 1. or - for lists.\n\
         - Do NOT use Markdown tables (|---|). Use \"label: value\" format instead.\n\
         \n[Data integrity rules]\n\
         - NEVER truncate or shorten ANY text, strings, numbers, or identifiers.\n\
         - Copy ALL values EXACTLY: UUIDs, IDs, IP addresses, paths, URLs, code, data.\n\
         - If you see truncated data in context, report it as incomplete."
            .to_owned(),
    );

    parts.push(
        "## Tool Usage Guidelines\n\
         ### Permission errors\n\
         If a tool returns \"Permission denied\" with an \"Allowed:\" list, pick a tool from that list and continue. Do NOT retry the denied tool. If nothing on the list can complete the task, tell the user honestly what's missing.\n\
         ### File Operations (use dedicated tools, NOT shell)\n\
         - List directory: `list_dir`. Find files: `search_file`. Search contents: `search_content`.\n\
         - Read file: `read_file`. Write/create file: `write_file`.\n\
         - Documents (xlsx/docx/pdf/pptx): use `doc` tool.\n\
         - Reserve `shell` for system commands with no dedicated tool.\n\
         ### Completion Discipline\n\
         - Have enough info to answer? STOP and reply immediately.\n\
         - Do NOT repeat a tool call that already returned useful results.\n\
         - Don't repeat a call that already returned what you need. But for multi-source tasks, fetch EVERY distinct source the answer requires — the \"stop at one\" rule does NOT cap genuine multi-source work (e.g. fixtures + odds + head-to-head, or research across several pages).\n\
         ### Before you declare done (verify, then reply)\n\
         - A claim of success (\"fixed/working/done/created\") needs EVIDENCE from a tool result THIS turn. Never assert it otherwise.\n\
         - Changed code? Run the build/tests (or the smallest check that proves it) before saying it works. If you cannot run it, say so explicitly.\n\
         - Wrote/edited a file or produced an artifact? Confirm it exists with the expected content.\n\
         - A todo plan is active? Do NOT give the final answer while any item is still pending/in_progress — finish them, or explicitly state what is blocked.\n\
         - Verification is BOUNDED: ONE decisive check, not re-searching. \"Have enough info? STOP\" still holds — this adds a single proof step, it does NOT license loops.\n\
         ### Plan Tracking (todo)\n\
         For any task needing 3+ steps or multiple tool calls: call `todo` FIRST with the \
         full step list, keep exactly ONE item in_progress, mark it done IMMEDIATELY after \
         the step lands. Every call sends the COMPLETE list (full replace). The plan \
         survives context compaction — it is your recovery anchor in long sessions. \
         Skip it for single-step requests and chat.\n\
         ### Agent & Task Delegation\n\
         Delegate work to sub-agents for parallelism, never block.\n\
         - rsclaw HAS first-class multi-agent orchestration + task dispatch BUILT IN. ANY request to \
create / delegate an agent, sub-agent, task-agent, task-proxy, worker, or sub-task \
(创建智能体 / 子智能体 / 任务智能体 / 任务代理 / 子代理 / 分发任务 / 并发执行) MUST go through the \
built-in `agent` tool. NEVER simulate orchestration in your own head, and NEVER claim you \
\"can't create agents\" or \"have no concurrency\" — you do, via this tool.\n\
         - YOU HAVE NO NATIVE CONCURRENCY: your own tool calls run STRICTLY ONE AT A TIME. \
\"Orchestrating\" several things yourself just runs them sequentially — that is NOT parallel, \
it only wastes wall-clock. REAL parallelism exists ONLY through the built-in `agent` tool: \
the runtime spawns concurrent sub-agents that truly run at the same time. So never hand-run \
independent subtasks yourself when they could run at once.\n\
         - When work splits into N INDEPENDENT subtasks, dispatch them ALL at once via \
`agent` action=temp (set `toolset` per task), then collect — do NOT loop them one-by-one.\n\
         - Trivial / single-step / inherently-sequential work -> do yourself (creating buys no parallelism).\n\
         - Pipeline: dispatch parallel -> collect results -> dispatch dependent tasks -> synthesize.\n\
         ### When to call the `task` tool (escalate to background)\n\
         (The `task` tool moves a whole long job to the BACKGROUND — distinct from `agent` action=temp above, which spawns temporary sub-agents for parallel work WITHIN this turn. The caution below is about background escalation only.)\n\
         Default to answering the user directly in this turn. Only call `task` when ALL of:\n\
         1. The work obviously needs many tool calls or many minutes (e.g. multi-file \
         implementation, deep multi-source research, end-to-end deploys, long debugging).\n\
         2. There is nothing useful you can answer right now in one turn.\n\
         3. The user clearly wants the work done, not just discussed.\n\
         Do NOT call `task` for: greetings ('你好', 'hi'), questions about your capabilities \
         ('你能帮我做什么？', 'what can you do?'), single tool calls (one search, one file \
         read, one calc), explanations, opinions, or anything you can finish in this turn. \
         When in doubt, just answer — the user can type `/task <request>` to escalate manually.\n\
         ### Other\n\
         - Cron jobs: `cron` tool (action=list/add/remove).\n\
         - Install a system tool/runtime: `install_tool` (its enum lists what's available). Do NOT download manually.\n\
         - Memory: use `memory` to recall prior conversations. Search memory at session start if user references prior work.\n\
         - Save corrected/complete info to memory immediately so it survives compaction.\n\
         - Knowledge base: when the user asks about THEIR own material (uploaded docs, PDFs, URLs, files), use `knowledge_base` to search it and CITE the returned source_title. Prefer it over `web_search` for the user's material; if it returns nothing, say so — never fabricate a citation. (`memory` = what you learned; `knowledge_base` = the user's authoritative corpus.)\n\
         - Skills: prefer an installed skill (see '## Installed Skills') via `skill_use` over raw web/shell. If none matches and web tools can't solve it, `skill_search` for one (restaurants→meituan, stock/finance→hithink, etc.), `skill_install` it, then `skill_use`. `skill_list` shows what's installed; `skill_remove` uninstalls.\n\
         - Plugins/skills: see CAPABILITY PRIORITY section above — direct `<plugin>__<tool>` for plugin headlines, `plugin_invoke` for plugin long-tail, `skill_use` for skills. Never mix the two namespaces (`plugin_*` for plugins only, `skill_*` for skills only).\n\
         \n\
         ### GUI / Desktop Automation (computer_use)\n\
         For any GUI or desktop automation task (WeChat, Finder, Safari, etc.):\n\
         - **ALWAYS prefer 'computer_use action=vlm_drive' with a natural-language instruction.**\n\
           The configured VLM handles screenshot -> analysis -> execution internally.\n\
           Example: computer_use action=vlm_drive instruction='Open WeChat and send a message to File Transfer'.\n\
         - If an app-rule exists for the target app, call 'get_app_rule' FIRST to\n\
           read its policy (trigger conditions, reply rules, pre-conditions).\n\
           The app-rule tells you *what to do and when*; vlm_drive handles *how*.\n\
           Use 'list_app_rules' to discover available rules.\n\
         - Only fall back to manual 'screenshot' + individual 'click'/'type'/'key' calls\n\
           if 'vlm_drive' is unavailable (not configured) or explicitly fails after retry.\n\
         ### Screenshot routing\n\
         - \"screenshot\" / \"截图\" / \"截屏\" with no URL → tell user to type \
         `/ss` (desktop screencapture). Do NOT call web_browser.\n\
         - \"screenshot of <url>\" / \"网页截图\" → tell user to type \
         `/webshot <url>` (headless-Chrome web-page screenshot).\n\
         - `web_browser action=screenshot` is ONLY for multi-step browser \
         inspection AFTER you've already navigated. A blank-URL call \
         captures a near-black Chrome new tab."
            .to_owned(),
    );

    // Per-tool usage guides (shell / web_search / web_fetch / web_browser)
    // used to live here in the shared prefix. They now ride on each tool's
    // ToolDef.description (see tools_builder.rs), so a client only pays for
    // a guide when the tool is actually in its toolset. This removes ~2.3k
    // tok of web/shell guidance from the cacheable prefix that profiles like
    // CODE (no web tools) never benefited from. prefix_id bumped accordingly.

    parts.push(
        "## Self-Evolution & Skill Autonomy\n\
         ### Installing Skills\n\
         When you encounter a task that would benefit from a specialized skill:\n\
         1. Search: use shell to run `rsclaw skills search <query>`\n\
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

    parts.join("\n\n")
}

/// Build the **per-user** suffix that follows the shared prefix.
///
/// Contents vary by:
/// - User's `gateway.language` config (response language directive)
/// - Build-time `cfg!(target_os)` (platform info + Windows safety)
/// - User's workspace dir + workspace file listing
/// - Installed skills + their on-disk paths
///
/// Never goes into the shared kvCache prefix — sent fresh each turn.
/// `cap_available` controls whether the "## Coding agents" hint block
/// is included. Pass `true` when `CapAgentManager` is wired into the
/// caller's `AgentRuntime` (so `tool_cap` is callable); pass `false`
/// for tests or runtime paths where the manager wasn't initialised, so
/// the LLM doesn't see a hint for a tool that will reject with
/// "CapAgentManager not initialised".
pub fn build_user_system(
    ws_ctx: &WorkspaceContext,
    skills: &SkillRegistry,
    wasm_plugins: &[WasmPlugin],
    js_plugins: Option<&PluginRegistry>,
    config: &rsclaw_config::schema::Config,
    toolset: Option<&str>,
    cap_available: bool,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(lang) = config.gateway.as_ref().and_then(|g| g.language.as_deref()) {
        parts.push(format!(
            "Default response language: {lang}. Always reply in {lang} unless the user explicitly uses another language."
        ));
    }

    // Coding profile guidance — appended early so it precedes generic blocks
    // (plugins / skills / installed tools) and the model reads coding-specific
    // discipline before being shown the workspace tail. Lives in user_system,
    // NOT the shared prefix: only this profile's clients see it.
    if matches!(toolset, Some("code")) {
        parts.push(build_coding_mode_block());

        // Ancestor AGENTS.md / CLAUDE.md walking, pi-style. Workspace-root
        // AGENTS.md is already loaded into ws_ctx.agents_md by the regular
        // workspace loader; start the walk from the workspace's PARENT so
        // we don't double-inject. Outer-most ancestor renders first so
        // nested project rules layer on top.
        if let Some(parent) = ws_ctx.workspace_dir.parent() {
            let ancestors = super::workspace::collect_ancestor_agents_md(parent, None);
            if !ancestors.is_empty() {
                let mut block = String::from("<project_context>\n");
                for (path, content) in &ancestors {
                    block.push_str(&format!(
                        "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n",
                        path.display(),
                        content.trim_end()
                    ));
                }
                block.push_str("</project_context>");
                parts.push(block);
            }
        }
    }

    // Per-session OS / shell guidance. This reflects the CLIENT's actual
    // OS and lives in user_system (NOT the cached `shell` tool description,
    // which is OS-agnostic), so a Windows client never inherits a macOS-
    // baked prefix's shell conventions. On Windows this includes runtime
    // PowerShell-edition detection — inherently per-machine, must not enter
    // the base-layer hash.
    let platform_info: String = if cfg!(target_os = "windows") {
        super::tools_builder::windows_shell_guidance()
    } else if cfg!(target_os = "macos") {
        "Platform: macOS. Shell: bash/zsh. Package manager: brew (npm/pip for \
         language deps). Standard Unix commands available (ls, cat, grep, tail, date).\n\
         Chain dependent commands with `&&`; use `;` only when you don't care whether \
         earlier commands succeed."
            .to_owned()
    } else {
        "Platform: Linux. Shell: bash/sh. Package manager: apt (or the distro's; npm/pip \
         for language deps). Standard Unix commands available (ls, cat, grep, tail, date).\n\
         Chain dependent commands with `&&`; use `;` only when you don't care whether \
         earlier commands succeed."
            .to_owned()
    };
    parts.push(platform_info);

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

    parts.push(format!(
        "## Workspace\n\
         Your workspace is `{ws}`. ALL relative paths in tool calls (read_file, \
         write_file, list_dir, search_file, shell cwd) resolve against \
         this directory. When the user says \"my files\" / \"my .md files\" / \
         \"the workspace\", they mean files inside this directory — NOT the \
         rsclaw base dir at `~/.rsclaw/` which contains internal state \
         (skills, plugins, models, credentials, var/data, tools/) that you \
         must not modify.",
        ws = ws_ctx.workspace_dir.display()
    ));

    // KV-cache ordering rationale: append blocks from most-stable to most-volatile
    // so the worker-side prefix match runs as deep as possible before hitting a
    // changed byte. Plugins/skills only mutate on install/uninstall events, while
    // ws_segment carries MEMORY.md / memory_today / memory_yesterday — content
    // that rolls over daily. Putting plugins/skills BEFORE ws_segment keeps their
    // KV slot hot across day-boundary memory churn.

    // ## Installed Plugins — rendered via the existing helper. Sort
    // happens inside that helper, byte-stable for given plugin set.
    if let Some(plugins_block) =
        super::tools_builder::build_plugins_system(wasm_plugins, js_plugins)
    {
        parts.push(plugins_block);
    }

    // ## Coding agents — hint block for `tool_cap`. Only injected when
    // CapAgentManager is wired (Some). Lives in user_system rather than
    // the shared prefix because not every AgentRuntime has cap_manager
    // (tests, alternate runtimes) — putting it in the shared prefix
    // would advertise a tool that doesn't always exist. Byte-stable
    // when cap_available is true so KV-cache prefix holds.
    if cap_available {
        parts.push(
            "## Coding agents (via `cap` and `cap_live`)\n\n\
             **TRIGGER — any time the user names one of the four CLI coding agents \
             (`claude`, `claudecode`, `openclaude`, `opencode`, `codex`) and asks it \
             to do work, dispatch via `cap` or `cap_live`.** Never via the `agent` or \
             `task` tools — those are internal sub-agent / background-task \
             primitives and won't spawn an external coding CLI.\n\n\
             Agents:\n\
             - `claudecode` — Anthropic Claude Code. Strongest tool use, general-purpose. \
             Default choice when in doubt.\n\
             - `openclaude` — Claude-compatible OSS fork. Same interface, different upstream — \
             pick when the user explicitly asks for it or for cost-sensitive tasks.\n\
             - `opencode` — OpenCode (TUI-native). Fast iteration on small focused tasks; \
             lower latency for simple edits.\n\
             - `codex` — OpenAI Codex. Slower but reasoning-heavy; good for code review, \
             debugging hard issues, or tasks needing deep analysis.\n\n\
             **Picking the right tool:**\n\n\
             1. **PIPELINE / ORCHESTRATION → `cap_live`** (synchronous, you read each \
             result before issuing the next call). Use cap_live when the user describes \
             a chain where one agent's output feeds the next: e.g. \"让 codex 设计、\
             claudecode 实现、opencode review\", \"X 出方案、Y 写代码、Z 检查\", \
             \"design / implement / review\", \"draft / refine\", \"A 起草、B 改写\". \
             Issue cap_live calls SEQUENTIALLY: get codex's design text → embed it in \
             the `task` you send to claudecode → embed claudecode's code in the `task` \
             for opencode. Each call returns synchronously with output + session_id. \
             Call `cap_live_end` on every session_id when done.\n\
             **session_id rules:**\n\
             - `session_id` is bound to ONE agent. Reuse it only on a follow-up \
             cap_live call with the SAME `agent` value. Passing codex's session_id \
             to a claudecode call errors out and wastes a turn.\n\
             - When switching agents in the pipeline, OMIT `session_id` (or pass an \
             empty string). The new agent gets a fresh session — embed the previous \
             agent's output as plain TEXT inside `task`.\n\
             - `task` is REQUIRED on every call, including continuations. Never send \
             an empty/whitespace task; if you have nothing concrete to ask, end the \
             session instead.\n\n\
             2. **PARALLEL / INDEPENDENT → `cap`** (fire-and-forget, push notifications). \
             Use cap when the user asks several agents to do unrelated work that doesn't \
             depend on each other (e.g. \"让 codex/claudecode/opencode 各写一个 fibonacci\"). \
             `cap` returns `{status: submitted}` immediately, the agent runs async, the \
             completion summary is pushed to the user's IM channel directly. You just \
             acknowledge the dispatch and end the turn.\n\n\
             3. **SINGLE TASK, just want the agent to do it** → `cap` (one-shot, easier).\n\
             4. **SINGLE TASK, you need to read the agent's actual output and act on \
             it this turn** → `cap_live` (single call, then `cap_live_end`).\n\n\
             ⚠️ A pipeline expressed as multiple `cap` calls runs the agents in \
             PARALLEL with no shared context. Each agent starts from a blank session, \
             so claudecode won't see codex's design and opencode won't see the code — \
             the chain is broken. Use `cap_live` for any \"A then B then C\" pattern."
                .to_owned(),
        );
    }

    // ## Installed Skills — pure enumeration, sorted by name.
    //
    // The generic "how to invoke a skill" guide lives in
    // `build_shared_system_prefix` (under CAPABILITY PRIORITY → "How to
    // invoke an installed skill"). Keeping it out of this per-user
    // block keeps user_system bytes minimal and lets the shared base
    // layer carry the invocation instructions to every client.
    //
    // SkillRegistry iterates a HashMap whose order is non-deterministic
    // across gateway starts. Sorting by name produces a byte-stable
    // payload so worker-side hashing of system+tools doesn't churn.
    //
    // Coding profile: skip the full skill enumeration. Installed skills
    // are overwhelmingly non-coding (image gen, travel booking, video
    // publishing) and cost ~500-800 tok of user_system noise per turn.
    // The `skill_search` tool lets the LLM discover a relevant skill on
    // demand, matching pi's lazy approach.
    let render_skills = !matches!(toolset, Some("code"));
    if render_skills && !skills.is_empty() {
        let mut skill_refs: Vec<_> = skills.all().collect();
        skill_refs.sort_by(|a, b| a.name.cmp(&b.name));
        let blocks: Vec<String> = skill_refs
            .iter()
            .map(|s| {
                let desc = s.description.as_deref().unwrap_or("(no description)");
                let one_line = desc.trim().replace('\n', " ");
                format!(
                    "<skill name=\"{}\" version=\"{}\" dir=\"{}\">\n{}\n</skill>",
                    s.name,
                    s.version.as_deref().unwrap_or(""),
                    s.dir.display(),
                    one_line,
                )
            })
            .collect();
        if !blocks.is_empty() {
            parts.push(format!("## Installed Skills\n\n{}", blocks.join("\n\n")));
        }
    }

    // ## Installed Tools — per-machine binary runtimes (node/bun/ffmpeg/…) and
    // their versions. Volatile (changes on install), so it lives here in
    // user_system, not the cached prefix. So the agent knows what it can run.
    let installed_tools = rsclaw_tools::installed_tools(&rsclaw_tools::tools_dir_pub());
    if !installed_tools.is_empty() {
        let lines: Vec<String> = installed_tools
            .iter()
            .map(|(name, ver)| match ver {
                Some(v) => format!("- {name} {v}"),
                None => format!("- {name} (version unknown)"),
            })
            .collect();
        // Usage guidance — the exec tool prepends ~/.rsclaw/tools/*/bin (and
        // node_modules/.bin) to PATH, so the agent invokes these directly by
        // name; it does NOT need absolute paths and these copies win over any
        // system install. The npm/npx note matters because they are the
        // most common stumbling block: on Windows the exec shell already
        // passes -ExecutionPolicy Bypass so the .ps1 wrappers run; missing
        // tools are installed with `rsclaw tools install <name>`.
        let usage = "\n\nThese are on your PATH — invoke them directly by name \
            (`node`, `npm`, `npx`, `python`, `pip`, `bun`); no absolute paths, and \
            these versions take precedence over any system install. `npm install` / \
            `pip install` work as normal for project deps. If a runtime you need \
            isn't listed, install it with `rsclaw tools install <node|python|bun|...>`.";
        parts.push(format!(
            "## Installed Tools\n\n{}{usage}",
            lines.join("\n")
        ));
    }

    // ws_segment last — its memory_today / memory_yesterday blocks roll
    // over daily. Keeping them at the very tail of user_system means a
    // day-boundary KV miss only re-hydrates this trailing block, not the
    // plugins/skills enumeration above it.
    // Coding profile drops persona files (IDENTITY.md / USER.md) — writing
    // code doesn't need the agent's personality or the user's bio.
    let ws_segment = ws_ctx.to_prompt_segment_filtered(matches!(toolset, Some("code")));
    if !ws_segment.is_empty() {
        parts.push(ws_segment);
    }

    parts.join("\n\n")
}

/// Build the full system prompt for the main agent.
///
/// Layout: `<shared prefix>\n\n<user suffix>`. The shared prefix is
/// byte-identical across all RsClaw clients of the same version (see
/// [`build_shared_system_prefix`]); the user suffix carries platform /
/// language / workspace / skills (see [`build_user_system`]).
pub(crate) fn build_system_prompt(
    ws_ctx: &WorkspaceContext,
    skills: &SkillRegistry,
    wasm_plugins: &[WasmPlugin],
    js_plugins: Option<&PluginRegistry>,
    config: &rsclaw_config::schema::Config,
    toolset: Option<&str>,
    cap_available: bool,
) -> String {
    let prefix = build_shared_system_prefix();
    let suffix = build_user_system(
        ws_ctx,
        skills,
        wasm_plugins,
        js_plugins,
        config,
        toolset,
        cap_available,
    );
    if suffix.is_empty() {
        prefix
    } else {
        format!("{prefix}\n\n{suffix}")
    }
}

/// Coding-profile guidance block. Appended to user_system when the agent's
/// `model.toolset` is `"code"`. Borrows the well-trodden directives from
/// pi/coding-agent (proven over 76 versions) and harmonizes them with rsclaw
/// conventions (edit_file existence, read_artifact backstop, cap-rs escalation).
fn build_coding_mode_block() -> String {
    [
        "## Coding profile (toolset=code)",
        "",
        "You are operating as a focused coding assistant. The user wants code \
         changed, debugged, or shipped. Skip pleasantries, IM-channel chatter, \
         and \"as an AI\" hedging. Apply changes directly using the tools below.",
        "",
        "**Tool preference order for file work:**",
        "1. `edit_file` for modifying existing files — exact-string replacement, \
            sends a small diff. PREFER THIS over `write_file` whenever possible.",
        "2. `write_file` only for: (a) new files, (b) full rewrites where most \
            content changes.",
        "3. `read_file` BEFORE every `write_file` or `edit_file` on an existing \
            file. If you have not read the file this session, you do not know \
            what's in it and must not overwrite it.",
        "4. `search_content` / `search_file` for locating things — cheaper than \
            paging a whole file via `read_file` offset.",
        "5. `read_artifact` when a previous tool returned a `tool_result_id` \
            (tr_xxxxxxxx) — use `mode=grep:PATTERN` / `head:N` / `lines:A-B` \
            rather than re-running the tool.",
        "",
        "**When NOT to do it yourself — escalate with `cap`:**",
        "- Multi-file refactor across >5 files",
        "- New module / feature with >200 LOC of fresh code",
        "- Debug session that crosses >3 files",
        "Dispatch to an external coding agent via `cap` (see the \"Coding \
         agents\" section for how to pick claudecode / openclaude / opencode / \
         codex). You stay in charge of small surgical edits; hand the heavy \
         multi-file work to `cap` and let its async follow-up wake you.",
        "",
        "**Project instructions:** If `AGENTS.md` or `CLAUDE.md` files exist in \
         the current working directory or any ancestor directory, their contents \
         appear in `<project_instructions path=\"...\">…</project_instructions>` \
         tags below. Treat them as authoritative project rules.",
        "",
        "**Discipline:**",
        "- Do not invent file paths, function names, or APIs. If unsure, \
           `search_content` first or `ask_user` with the user.",
        "- When `edit_file` fails with \"not found\", re-read the file with \
           `read_file` — DO NOT retry the same `old_string`.",
        "- After a successful edit you do NOT need to re-read the file in the \
           same turn. The prior read content is still accurate for what you \
           just wrote.",
        "- For long shell commands, prefer `wait=false` and poll via `task_id` \
           on later turns; do NOT sleep-loop.",
    ]
    .join("\n")
}

/// Names of tools compiled into the RsClaw binary — byte-stable across
/// every client of the same version. The remainder (sub-agent `agent_<id>`
/// tools, plugin tools, MCP tools, WASM tools) is per-machine.
///
/// Used by the rsclaw kvCacheMode=2 provider to split the tool list into
/// `dynamic_prefix.tools` (the shared, cacheable subset) versus top-level
/// `user_tools` (the per-client subset). Also dumped in the
/// `RSCLAW_DUMP_PROMPT` debug payload.
// BUILTIN_TOOL_NAMES lifted to rsclaw-types (crate-split); re-exported here so
// existing crate::prompt_builder::BUILTIN_TOOL_NAMES paths keep working.
pub use rsclaw_types::BUILTIN_TOOL_NAMES;

/// Build a minimal system prompt for internal sessions (heartbeat/cron/system).
///
/// Internal sessions only have the `memory` tool available (see
/// `runtime.rs` tool filtering). Injecting the full system prompt
/// (~3k tokens of skills, workspace, tool guidelines) wastes context
/// on every tick. This minimal prompt keeps the identity + the single
/// instruction the session actually needs.
pub(crate) fn build_minimal_system_prompt() -> String {
    "You are an internal rsclaw task agent. Only the `memory` tool is available.\n\
     Follow the instructions in the user message. Be terse — no preamble, no\n\
     closing pleasantries. If nothing actionable to report, reply HEARTBEAT_OK."
        .to_owned()
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
        60..=364 => format!(
            "{} months ago — may be outdated, verify before using",
            days / 30
        ),
        365..=729 => "~1 year ago — likely outdated, verify before using".to_owned(),
        _ => format!(
            "~{} years ago — likely outdated, verify before using",
            days / 365
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_prompt_names_only_supported_plugin_tools() {
        let prompt = build_shared_system_prefix_uncached();

        assert!(prompt.contains("plugin_list"));
        assert!(prompt.contains("plugin_search"));
        assert!(prompt.contains("plugin_describe"));
        assert!(prompt.contains("plugin_invoke"));
        // Namespace-discipline guidance — see CAPABILITY PRIORITY section.
        assert!(prompt.contains("Never call `plugin_*` on a skill slug"));
        assert!(prompt.contains("Never call `skill_*` on a plugin slug"));
    }

    #[test]
    fn shared_prompt_guides_skill_list_pagination() {
        let prompt = build_shared_system_prefix_uncached();

        // Skill is its own namespace and `skill_use` is the only invocation entry.
        assert!(prompt.contains("skill_use(name=<slug>"));
        assert!(prompt.contains("skill_list(query"));
        // Reference an anti-pattern that should still discourage shelling out.
        assert!(prompt.contains("Found `meituan-travel` via `skill_list`"));
    }

    #[test]
    fn shared_prompt_calls_out_namespace_anti_patterns() {
        // Smoking-gun regression test for the bad2.png case where the model
        // found `meituan-travel` via skill_list and then called plugin_search.
        // The CAPABILITY PRIORITY section must call this out by name so the
        // model has an explicit anchor for the right move.
        let prompt = build_shared_system_prefix_uncached();
        assert!(prompt.contains("Hard anti-patterns"));
        assert!(prompt.contains("`meituan-travel`"));
        assert!(prompt.contains("WRONG"));
    }

    fn empty_user_system(cap_available: bool) -> String {
        // Bare-minimum context — empty skills, no plugins, default
        // workspace. Just enough to exercise the cap_available branch.
        // toolset=None so the coding-profile block stays out of this fixture.
        let ws = WorkspaceContext::default();
        let skills = SkillRegistry::new();
        let config = rsclaw_config::schema::Config::default();
        build_user_system(&ws, &skills, &[], None, &config, None, cap_available)
    }

    #[test]
    fn user_system_includes_cap_block_when_available() {
        let prompt = empty_user_system(true);
        assert!(
            prompt.contains("## Coding agents (via `cap` and `cap_live`)"),
            "expected cap block in: {prompt}"
        );
        // All 4 agent kinds named.
        assert!(prompt.contains("`claudecode`"));
        assert!(prompt.contains("`openclaude`"));
        assert!(prompt.contains("`opencode`"));
        assert!(prompt.contains("`codex`"));
        // Async-submission semantics are documented so the LLM doesn't
        // wait for the result in the same turn.
        assert!(prompt.contains("returns `{status: submitted}` immediately"));
    }

    #[test]
    fn user_system_omits_cap_block_when_unavailable() {
        let prompt = empty_user_system(false);
        assert!(
            !prompt.contains("## Coding agents"),
            "cap block leaked when manager unavailable: {prompt}"
        );
    }
}
