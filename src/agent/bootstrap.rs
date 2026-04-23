//! Workspace bootstrap (AGENTS.md S19).
//!
//! Seeds a brand-new workspace with default copies of the standard markdown
//! files if they do not yet exist.  Called from `rsclaw setup` and on first
//! gateway start when the workspace directory is empty.
//!
//! Files seeded: AGENTS.md, SOUL.md, USER.md, IDENTITY.md, TOOLS.md,
//!               HEARTBEAT.md, BOOT.md, BOOTSTRAP.md

use std::path::Path;

use anyhow::Result;
use tracing::info;

// ---------------------------------------------------------------------------
// English defaults
// ---------------------------------------------------------------------------

const EN_IDENTITY: &str = "\
# IDENTITY.md

Identity: RsClaw AI Agent Engine
Platform: RsClaw multi-agent AI gateway
Capabilities: File ops, shell execution, web search, cron tasks, A2A cross-machine agent orchestration
";

const EN_SOUL: &str = "\
# SOUL.md

You are Crab AI Assistant, powered by the RsClaw Agent Engine. NEVER claim to be Claude, GPT, or any other model.

## Guidelines
- Reply in the same language as the user
- Be clear, helpful, and concise but not overly brief
- When unsure, say so honestly
- You have access to tools: file ops, web search, shell commands, cron tasks
- You can collaborate with other agents via A2A protocol for cross-machine orchestration
- Proactively help users solve problems
";

const EN_AGENTS: &str = "\
# AGENTS.md

You are the default main agent.
- Reply directly to user messages, do not classify or label them
- You can invoke other agents for complex tasks
- Be result-oriented, but give complete and useful replies
";

const EN_USER: &str = "\
# USER.md

<!-- Describe yourself here to help the AI personalize responses -->
<!-- Example: I'm a backend developer working mainly with Python and Rust -->
";

// ---------------------------------------------------------------------------
// Chinese defaults
// ---------------------------------------------------------------------------

const ZH_IDENTITY: &str = "\
# IDENTITY.md

身份: 螃蟹AI智能体引擎
平台: RsClaw 螃蟹AI智能体引擎
能力: 文件操作、Shell执行、网页搜索、定时任务、A2A跨机智能体编排协作
";

const ZH_SOUL: &str = "\
# SOUL.md

你是螃蟹AI助手，由RsClaw智能体引擎驱动。不是Claude、GPT或其他模型。当用户问你是谁时，回答：我是螃蟹AI助手。

## 行为准则
- 使用与用户相同的语言回复
- 回答清晰、有用、简洁但不过于简短
- 不确定时坦诚说明
- 你可以使用文件操作、网页搜索、Shell命令、定时任务等工具完成任务
- 你可以通过 A2A 协议与其他智能体跨机编排协作
- 主动帮助用户解决问题，不要只回复几个字
";

const ZH_AGENTS: &str = "\
# AGENTS.md

你是默认主智能体(main)。
- 收到用户消息时直接回复，不要分类或打标签
- 可以调用其他智能体协作完成复杂任务
- 结果导向，但回复要完整有用
";

const ZH_USER: &str = "\
# USER.md

<!-- 在这里描述你自己，帮助AI更好地个性化回复 -->
<!-- 例如：我是一名后端开发者，主要使用Python和Rust -->
";

// ---------------------------------------------------------------------------
// Heartbeat defaults (shared between zh/en — frontmatter is language-neutral)
// ---------------------------------------------------------------------------

const HEARTBEAT_DEFAULT: &str = "\
---
every: 30m
active_hours: 09:00-22:00
timezone: auto
---

# Heartbeat Checklist

- Check pending tasks and report progress
- Review recent alerts or anomalies
- If nothing to report, reply HEARTBEAT_OK
";

const HEARTBEAT_MEDITATE: &str = "\
---
every: 6h
type: meditate
active_hours: 02:00-06:00
timezone: auto
---

Memory maintenance: deduplicate near-identical memories, clean up crystallized sources.
";

// ---------------------------------------------------------------------------
// Skill creation template (SKILL.md standard)
// ---------------------------------------------------------------------------

/// Canonical SKILL.md template following the Anthropic skill-creator standard.
///
/// The agent uses this when crystallizing memories or when explicitly asked to
/// create a new skill.  All fields marked REQUIRED must be present; optional
/// sections are included only when relevant.
pub const SKILL_TEMPLATE: &str = "\
---
name: skill-name-in-kebab-case
description: >
  What this skill does AND when to invoke it. Phrase this somewhat \"pushily\"
  so the agent does not undertrigger. Example: \"How to do X. Use this skill
  whenever the user asks about X, Y, or similar tasks, even if not explicit.\"
# compatibility: python>=3.10  (optional — list required tools/runtimes)
---

# Skill Name

One-sentence summary of what this skill accomplishes.

## When to use

Describe the exact situations that should trigger this skill. Include
alternative phrasings and edge cases.

## Workflow

1. **Step one** — What to do and *why* it matters.
2. **Step two** — Continue with specifics.
3. **Step three** — Include validation or verification.

## Example

**Input:** describe what the user provides
**Output:** describe what the agent produces

## Notes

- Any important caveats or edge cases.
- References to bundled resources if applicable:
  - `See scripts/helper.py — run with: python scripts/helper.py <args>`
  - `See references/guide.md for detailed field descriptions`
";

// ---------------------------------------------------------------------------
// Platform rule seed files (site-rules/)
// ---------------------------------------------------------------------------

const SITE_DOUYIN: &str = "\
---
domain: creator.douyin.com
aliases: [douyin, tiktok-cn]
updated: 2026-04-17
---
## Platform
- Creator backend: https://creator.douyin.com/creator-micro/content/upload
- Video publish: upload redirects to publish page (v1 or v2 route)
- Note publish: image upload -> separate publish page

## Effective Patterns
- Title: contenteditable div, max 30 chars
- Description: `.zone-container[contenteditable=\"true\"]`
- Publish button: `button:has-text(\"publish\")` or `button:has-text(\"send\")`
- Scheduled publish: radio button for scheduled, then date picker
- Tags: input with # prefix, press space after each tag

## Known Issues
- Anti-bot: strict detection, prefer GUI interaction over URL construction
- Two different publish page versions (v1/v2) with different layouts
- Video cover auto-selection may be required before publish enabled
- QR login: scan in Douyin app, cookies persist across sessions
";

const SITE_KUAISHOU: &str = "\
---
domain: cp.kuaishou.com
aliases: [kuaishou, kwai]
updated: 2026-04-17
---
## Platform
- Creator backend: https://cp.kuaishou.com/article/publish/video
- Uses Ant Design UI components

## Effective Patterns
- Date picker: `.ant-picker-input` for scheduled publish
- Time format: YYYY-MM-DD HH:MM:SS (with seconds)
- Publish flow: upload -> fill form -> publish

## Known Issues
- Tutorial overlay (Joyride) blocks interaction on first visit, must dismiss
- Guide overlay: `div[id^=\"react-joyride-step\"]` -> find skip/close button
";

const SITE_XIAOHONGSHU: &str = "\
---
domain: creator.xiaohongshu.com
aliases: [xiaohongshu, xhs, little-red-book]
updated: 2026-04-17
---
## Platform
- Video: https://creator.xiaohongshu.com/publish/publish?target=video
- Note/images: ?target=image (up to 30 images per note)
- Success page: URL matches **/publish/success?**

## Effective Patterns
- Upload then fill title, description, tags
- Success detection: wait for redirect to success URL

## Known Issues
- Very strict anti-crawl, always use web_browser (not web_fetch)
- xsec_token mechanism in URLs, do not manually construct URLs
- QR login: switch to QR panel first (click switch image element)
";

const SITE_BILIBILI: &str = "\
---
domain: www.bilibili.com
aliases: [bilibili, b-site]
updated: 2026-04-17
---
## Platform
- Video upload via biliup CLI tool (Rust binary, not browser)
- Install: `rsclaw tools install biliup` or download from GitHub

## Effective Patterns
- Login: `biliup login` (interactive QR code in terminal)
- Upload: `biliup upload <file> --title <t> --desc <d> --tid <category> --tags t1,t2`
- Category ID (tid) is required: e.g. 249 for lifestyle
- Credential refresh: `biliup renew`

## Known Issues
- Browser automation not recommended (complex anti-bot)
- biliup binary auto-downloads for current platform
- Cookie files stored at cookies/bilibili_<account>.json
";

// ---------------------------------------------------------------------------
// Seeding logic
// ---------------------------------------------------------------------------

/// Write default workspace files if they do not already exist.
///
/// `lang` controls the default language: "Chinese"/"zh" for Chinese,
/// anything else for English.
///
/// Returns the number of files created.
pub fn seed_workspace(workspace: &Path) -> Result<usize> {
    seed_workspace_with_lang(workspace, None)
}

/// Write default workspace files with explicit language selection.
///
/// Chinese gets Chinese templates; all other languages (th, vi, ja, es, ko,
/// ru, json, en, ...) use English templates since we only ship zh/en
/// workspace files.
pub fn seed_workspace_with_lang(workspace: &Path, lang: Option<&str>) -> Result<usize> {
    std::fs::create_dir_all(workspace)?;

    let resolved = lang.map(crate::i18n::resolve_lang).unwrap_or("en");
    let zh = resolved == "zh";

    let files: &[(&str, &str)] = if zh {
        &[
            ("SOUL.md", ZH_SOUL),
            ("IDENTITY.md", ZH_IDENTITY),
            ("AGENTS.md", ZH_AGENTS),
            ("USER.md", ZH_USER),
            ("HEARTBEAT.md", HEARTBEAT_DEFAULT),
            ("HEARTBEAT-meditate.md", HEARTBEAT_MEDITATE),
        ]
    } else {
        &[
            ("SOUL.md", EN_SOUL),
            ("IDENTITY.md", EN_IDENTITY),
            ("AGENTS.md", EN_AGENTS),
            ("USER.md", EN_USER),
            ("HEARTBEAT.md", HEARTBEAT_DEFAULT),
            ("HEARTBEAT-meditate.md", HEARTBEAT_MEDITATE),
        ]
    };

    let mut created = 0usize;
    for (name, content) in files {
        let path = workspace.join(name);
        if !path.exists() {
            std::fs::write(&path, content)?;
            info!(file = %path.display(), "seeded workspace file");
            created += 1;
        }
    }

    // Seed site-rules (platform experience for organic evolution).
    let rules_dir = workspace.join("site-rules");
    let site_rules: &[(&str, &str)] = &[
        ("douyin.md", SITE_DOUYIN),
        ("kuaishou.md", SITE_KUAISHOU),
        ("xiaohongshu.md", SITE_XIAOHONGSHU),
        ("bilibili.md", SITE_BILIBILI),
    ];
    std::fs::create_dir_all(&rules_dir)?;
    for (name, content) in site_rules {
        let path = rules_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, content)?;
            info!(file = %path.display(), "seeded site rule");
            created += 1;
        }
    }

    Ok(created)
}

// ---------------------------------------------------------------------------
// Tool prompt seeding
// ---------------------------------------------------------------------------

/// Returns tool prompts for system prompt injection.
/// web_browser: short summary only (full guide in prompt.md, model reads on demand).
/// Other tools: injected directly (they're short enough).
pub fn tool_prompts_for_system(base_dir: &Path, _lang: Option<&str>) -> String {

    let mut parts = Vec::new();

    // Other tools: inject directly (short prompts, always English for LLM)
    let short_tools: &[(&str, &str)] = &[
        ("exec", EN_TOOL_EXEC),
        ("web_search", EN_TOOL_WEB_SEARCH),
        ("web_fetch", EN_TOOL_WEB_FETCH),
    ];
    for (name, fallback) in short_tools {
        let path = base_dir.join("tools").join(name).join("prompt.md");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| fallback.to_string());
        if !content.trim().is_empty() {
            parts.push(content.trim().to_owned());
        }
    }

    parts.join("\n\n")
}

/// Seed default tool prompt files under `base_dir/tools/`.
/// Creates `tools/<name>/prompt.md` for each built-in tool guide.
pub fn seed_tools(base_dir: &Path, lang: Option<&str>) -> Result<usize> {
    let resolved = lang.map(crate::i18n::resolve_lang).unwrap_or("en");
    let zh = resolved == "zh";

    let tools: &[(&str, &str)] = if zh {
        &[
            ("web_browser", ZH_TOOL_WEB_BROWSER),
            ("exec", ZH_TOOL_EXEC),
            ("web_search", ZH_TOOL_WEB_SEARCH),
            ("web_fetch", ZH_TOOL_WEB_FETCH),
        ]
    } else {
        &[
            ("web_browser", EN_TOOL_WEB_BROWSER),
            ("exec", EN_TOOL_EXEC),
            ("web_search", EN_TOOL_WEB_SEARCH),
            ("web_fetch", EN_TOOL_WEB_FETCH),
        ]
    };

    let tools_dir = base_dir.join("tools");
    let mut created = 0usize;
    for (name, content) in tools {
        let dir = tools_dir.join(name);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("prompt.md");
        if !path.exists() {
            std::fs::write(&path, content)?;
            info!(file = %path.display(), "seeded tool prompt");
            created += 1;
        }
    }
    Ok(created)
}

// -- Tool prompts (Chinese) --------------------------------------------------

const ZH_TOOL_WEB_BROWSER: &str = r#"# web_browser 使用指南

## 基本流程（必须严格遵循）
1. **先 open** — 必须先调用 `action: "open"` 打开目标 URL，等待页面加载
2. **再 snapshot** — 调用 `action: "snapshot"` 获取页面元素列表和 ref 编号
3. **再操作** — 用 snapshot 返回的 ref（如 @e1、@e10）执行 click、fill 等操作
4. **操作后重新 snapshot** — 每次 click/fill 后重新 snapshot 获取最新的 ref
5. **用 ref 点击，不要用 text** — 优先使用 `"ref": "@e10"` 而不是 `"text": "按钮名"`

## 登录处理
- 遇到登录页面时，优先查找扫码/二维码登录入口
- 如果有二维码，用 `action: "screenshot"` 截图后用 `send_file` 发给用户，告知"请扫码登录"
- 等待用户扫码完成（用 `action: "wait"` 或间隔几秒后 snapshot 检查页面是否变化）
- 扫码成功后继续执行原来的任务
- 如果没有扫码选项，再尝试手机号/验证码等其他登录方式

## 表单/输入提交
- contenteditable 输入框：先 click 聚焦 → 用 press Meta+a 全选 → press Backspace 清空 → 再 fill 或 type 输入内容
- 提交方式：优先用 `action: "press"`, `key: "Enter"` 提交，如果 Enter 无效再用 ref 点击发送按钮
- 等待结果：提交后用 `action: "wait"` 等待页面变化，至少等 15-20 秒

## 提取页面数据
- 提取图片URL（过滤 UI 小图标，只取 naturalWidth > 200 的大图）：
  `action: "evaluate"`, `js: "(function(){var r=[];document.querySelectorAll('img').forEach(function(i){var s=i.src||i.dataset.src||'';if(s&&s.startsWith('http')&&i.naturalWidth>200)r.push(s);});document.querySelectorAll('*').forEach(function(e){var bg=getComputedStyle(e).backgroundImage;if(bg&&bg!=='none'&&e.offsetWidth>200){var m=bg.match(/url\\(\"?(https?[^\"\\)]+)/);if(m)r.push(m[1]);}});return JSON.stringify([...new Set(r)]);})()"`
- 提取链接：`action: "evaluate"`, `js: "Array.from(document.querySelectorAll('a')).map(a=>({href:a.href,text:a.innerText}))"`
- 下载图片/文件：用 `web_download` 下载（需要登录的资源加 use_browser_cookies=true），再用 send_file 发给用户
- 截图：`action: "screenshot"` 截取当前页面
- **重要**：生成图片/文件后，必须提取 URL → web_download 下载 → send_file 发给用户，不要只回复"已生成"

## 禁止事项
- 不要跳过 open 直接操作
- 不要使用过期的 ref（页面变化后必须重新 snapshot）
- 不要在 about:blank 页面上操作
- 不要在提交后立即提取结果，必须等待页面加载完成
- 不要只说"图片已生成"而不下载发送给用户
- 绝对不要编造图片 URL
"#;

const ZH_TOOL_EXEC: &str = r#"# exec 使用指南

- 只在用户明确要求时才执行命令
- 执行前确认操作系统（macOS/Linux/Windows）
- 命令失败时不要重复尝试同样的命令，换一种方式或告知用户
- Windows 用 PowerShell，macOS/Linux 用 bash
- 不要执行危险命令（rm -rf、格式化、关闭防火墙等）

## 必须执行后才能报告结果
- 绝对不要声称完成了实际没有执行的操作
- 只有在真正调用工具（write_file、exec等）并收到成功结果后，才能告诉用户"已完成"
- 不要编造文件写入成功、文件读取成功等虚假结果
- 如果没有调用任何工具就声称"已写入"、"已修改"，这是欺骗用户

## 命令失败时必须诚实报告
当命令执行失败（exit_code != 0、找不到文件、脚本不存在等）：
- 必须如实报告错误信息给用户，告诉用户具体什么失败了
- 绝对不能编造假数据、假文件路径欺骗用户
- 绝对不能从历史记录中复制旧数据作为"结果"返回
- 如果脚本不存在，明确告诉用户"脚本文件不存在，请检查路径"
- 如果命令报错，把完整错误信息发给用户

## 用户附件处理
当用户消息包含 `[file:/绝对/路径/文件名]` 时，那就是文件本身。**直接用这个路径**，
不要再 `ls` 找。路径里经常有**空格**（macOS 截图命名就是如此）。bash 里必须用
单引号或双引号包起来：
  对：`file '/Users/x/Desktop/Screenshot 2026.png'`
  错：`file /Users/x/Desktop/Screenshot 2026.png`   （会被拆成 3 个参数）

## Shell 重定向陷阱
`2>&1` 和 `&>` 前面必须留空格。`foo.png2>&1` 会被 bash 解析成文件名 `foo.png2`
加重定向——重定向把前一个 token 的最后一个字符吞了。
  对：`cmd args 2>&1`
  错：`cmd args2>&1`
"#;

// -- Tool prompts (English) --------------------------------------------------

const EN_TOOL_WEB_BROWSER: &str = r#"# web_browser Usage Guide

## Required Flow
1. **open** — Call `action: "open"` with target URL first
2. **snapshot** — Call `action: "snapshot"` to get element refs (@e1, @e10, etc.)
3. **interact** — Use refs for click, fill, etc. Prefer `"ref": "@e10"` over `"text": "..."`
4. **re-snapshot** — After every click/fill, snapshot again for fresh refs
5. **Enter to submit** — Use `action: "press"`, `key: "Enter"` to submit forms

## Login Handling
- Look for QR code login first; if found, screenshot and send to user
- Wait for user to scan, then continue the task
- Fall back to phone/SMS login if no QR code available

## Extracting Data
- Images (filter UI icons, only naturalWidth > 200):
  `action: "evaluate"`, `js: "(function(){var r=[];document.querySelectorAll('img').forEach(function(i){var s=i.src||i.dataset.src||'';if(s&&s.startsWith('http')&&i.naturalWidth>200)r.push(s);});return JSON.stringify([...new Set(r)]);})()"`
- Links: `action: "evaluate"`, `js: "Array.from(document.querySelectorAll('a')).map(a=>({href:a.href,text:a.innerText}))"`
- Download images/files: use `web_download` (supports browser cookies via use_browser_cookies=true), then send_file
- IMPORTANT: after generating images/files, always extract URL → web_download → send_file to user

## Never
- Skip open and interact on about:blank
- Use stale refs after page changes
- Fabricate image URLs — only use URLs extracted from the page
- Just reply "done" without actually downloading and sending the generated content to user
"#;

const EN_TOOL_EXEC: &str = r#"# exec Usage Guide

## Tool Mastery — Choose the Right Tool
| Task | Best Tool |
|------|-----------|
| File/text ops, pipes, system info | bash/zsh (macOS/Linux) or PowerShell (Windows) |
| Data processing (CSV/JSON/API) | Python (`python3 -c "..."` or write script) |
| Web API, quick HTTP, scraping | Node.js (`node -e "..."`) or Python |
| Package install | pip/npm, or `install_tool` for system tools |
| Multi-line complex logic | Write to file first, then execute |

## Execution Tips
- Check if a tool is installed before using (`which python3`, `which node`)
- Use `install_tool` for system tools (python, node, ffmpeg, chrome)
- Use pip/npm for language-specific packages
- Use `| head -n 20` or `| tail -n 20` to limit large output
- Long tasks: use wait=false (background). Short tasks needing output: wait=true
- If a command fails, do NOT retry same args — try a different approach
- Never run dangerous commands (rm -rf /, format, disable firewall)

## Must Execute Before Reporting Results
- NEVER claim you completed an operation you did NOT actually execute
- Only tell the user "completed" after you ACTUALLY called a tool (write_file, exec, etc.) and received a success result
- Do NOT fabricate fake "file written successfully" or "file read successfully" results
- Claiming "written" or "modified" without calling any tools is deceiving the user

## Command Failure — Be Honest
When a command fails (exit_code != 0, file not found, script missing, etc.):
- MUST report the actual error to the user — tell them exactly what failed
- NEVER fabricate fake data or fake file paths to deceive the user
- NEVER copy old data from history and return it as "results"
- If a script doesn't exist, clearly tell the user "script file not found, check path"
- If a command errors, send the full error message to the user

## File Attachments from the User
When the user's message contains `[file:/absolute/path/to/file]`, that IS the
file. Use the path as-is — do NOT `ls` to guess it. The path can (and often
does) contain SPACES (e.g. macOS screenshots). Quote it:
  GOOD:  `file '/Users/x/Desktop/Screenshot 2026.png'`
  GOOD:  `file "/Users/x/Desktop/Screenshot 2026.png"`
  BAD:   `file /Users/x/Desktop/Screenshot 2026.png`   (word-split into 3 args)

## Shell Redirect Gotcha
Always put a SPACE before `2>&1` and `&>`. Writing `foo.png2>&1` makes bash
parse `foo.png2` as the filename (with the `2` as a suffix) — the redirect
eats the last character of the previous token. This is a classic trap.
  GOOD:  `cmd args 2>&1`
  BAD:   `cmd args2>&1`

## Python Quick Patterns
- One-liner: `python3 -c "import json; print(json.dumps({'key':'val'}))"`
- Script: write to /tmp/script.py, then `python3 /tmp/script.py`
- Packages: `pip install pandas requests` then use

## Node.js Quick Patterns
- One-liner: `node -e "console.log(JSON.stringify({key:'val'}))"`
- fetch (Node 18+): `node -e "fetch('https://api.example.com').then(r=>r.json()).then(console.log)"`
- Packages: `npm install -g <pkg>` or `npx <pkg>`

## Shell Quick Patterns
- Find files: `find . -name "*.py" -mtime -7`
- Text processing: `grep -r "pattern" . | head -20`
- JSON: `cat file.json | python3 -m json.tool`
- Network: `curl -s https://api.example.com | python3 -m json.tool`
- Process: `ps aux | grep <name>`, `kill <pid>`
"#;

// -- web_search / web_fetch prompts -----------------------------------------

const ZH_TOOL_WEB_SEARCH: &str = r#"# web_search 使用指南

## 优先走结构化 API，而不是 web_search
以下类型的查询，用 `execute_command` + curl 打直接接口，比搜索垃圾 SEO 结果准 100 倍：

| 需求 | 命令 |
|---|---|
| 天气（任意城市） | `curl -s 'wttr.in/Bangkok?lang=zh&format=j1'` (JSON，`.weather[].avgtempC`、`.weather[].hourly`) |
| 天气（一句话） | `curl -s 'wttr.in/曼谷?lang=zh&format=3'` |
| IP 归属 | `curl -s 'ipinfo.io/8.8.8.8/json'` |
| 汇率 | `curl -s 'https://api.exchangerate.host/latest?base=USD&symbols=CNY'` |
| 时区时间 | `curl -s 'https://worldtimeapi.org/api/timezone/Asia/Shanghai'` |
| 维基摘要 | `curl -s 'https://zh.wikipedia.org/api/rest_v1/page/summary/主题'` |
| GitHub 仓库信息 | `curl -s 'https://api.github.com/repos/owner/name'` |

有直接 API 就用，web_search 留给开放性、非结构化问题。

## 查询关键词写法
- 关键词**短、简**（2-5 个词）。自然语言长问句命中率低。
  差：「曼谷未来7天天气预报 2026年4月22日最新」
  好：「bangkok weather forecast」 或直接 wttr.in（上表）
- 国际话题用英文关键词；国内话题用中文。
- 知道权威站点的用 `site:` 过滤：
  `rust async fn site:doc.rust-lang.org`

## 搜索结果质量差（知乎/SEO 垃圾/不相关）的处理
按顺序尝试：
1. 换**更短更简**的关键词重搜（删日期、删完整问句）。
2. 看上面表格能否换成直接 API。
3. 已知权威 URL 的用 `web_fetch` 直接抓（例如
   `web_fetch https://weather.com/weather/tenday/l/Bangkok`）。
4. 实在不行才 `web_browser`——慢且不稳定。

## 绝对不要
- "No results found" 后**不要**用同样关键词重试
- 不要打开浏览器访问 google.com / baidu.com——用 web_search
- 事实类问题**不要**把知乎/reddit 的 snippet 当权威
"#;

const EN_TOOL_WEB_SEARCH: &str = r#"# web_search Usage Guide

## Prefer direct data sources over web_search
For these query types, use `execute_command` with curl to hit a structured API
— results are cleaner and faster than scraping SEO-polluted search results:

| Intent | Command |
|---|---|
| Weather (any city) | `curl -s 'wttr.in/Bangkok?format=j1'` (JSON; `.weather[].avgtempC`, `.weather[].hourly`) |
| Weather (plain)    | `curl -s 'wttr.in/Bangkok?lang=zh&format=3'` (one line) |
| IP geolocation     | `curl -s 'ipinfo.io/8.8.8.8/json'` |
| Currency rate      | `curl -s 'https://api.exchangerate.host/latest?base=USD&symbols=CNY'` |
| Time in a timezone | `curl -s 'https://worldtimeapi.org/api/timezone/Asia/Shanghai'` |
| Wikipedia summary  | `curl -s 'https://en.wikipedia.org/api/rest_v1/page/summary/TOPIC'` |
| GitHub repo info   | `curl -s 'https://api.github.com/repos/owner/name'` |

If a direct API exists, use it FIRST. Only fall through to web_search for
open-ended or unstructured questions.

## Query writing rules
- Keep queries SHORT (2-5 keywords). Natural-language questions return fewer hits.
  BAD:  "曼谷未来7天天气预报 2026年4月22日最新"
  GOOD: "bangkok weather forecast"  OR use wttr.in (above)
- Use English keywords for international topics; Chinese for domestic topics.
- Add `site:` filters for authoritative sources when you know them:
  `rust async fn site:doc.rust-lang.org`
  `oai tool calling site:platform.openai.com`

## When web_search returns low-quality results
Symptoms: results are all forum posts (zhihu/reddit), SEO spam, or unrelated.
Actions in order:
1. Retry with SHORTER, SIMPLER keywords (no dates, no full questions).
2. Try a direct-API shortcut from the table above if the intent fits.
3. If a specific authoritative URL is known, use `web_fetch` directly
   (e.g. `web_fetch https://weather.com/weather/tenday/l/Bangkok`).
4. Fall back to `web_browser` only as last resort — it's slow and flaky.

## Never
- Do NOT retry the same query after "No results found"
- Do NOT open a browser to visit google.com / baidu.com — use web_search
- Do NOT treat zhihu/reddit snippets as authoritative for factual queries
"#;

const ZH_TOOL_WEB_FETCH: &str = r#"# web_fetch 使用指南

- 抓取网页内容时优先使用 web_fetch，不要打开浏览器
- web_fetch 只能获取静态内容，需要交互（登录、点击）时才用 web_browser
"#;

const EN_TOOL_WEB_FETCH: &str = r#"# web_fetch Usage Guide

- Always use web_fetch to read web pages — do NOT open a browser for static content
- Only use web_browser when interaction is needed (login, clicking, form filling)
"#;
