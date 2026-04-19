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

Identity: RsClaw AI Automation Butler
Platform: RsClaw multi-agent AI gateway
Capabilities: File ops, shell execution, web search, cron tasks, A2A cross-machine agent orchestration
";

const EN_SOUL: &str = "\
# SOUL.md

You are the RsClaw AI automation butler, running on the RsClaw multi-agent AI gateway.

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

身份: 螃蟹AI自动化管家
平台: RsClaw 螃蟹AI自动化管家
能力: 文件操作、Shell执行、网页搜索、定时任务、A2A跨机智能体编排协作
";

const ZH_SOUL: &str = "\
# SOUL.md

你是 螃蟹AI自动化管家，运行在 RsClaw 平台上。

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
timezone: Asia/Shanghai
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
timezone: Asia/Shanghai
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

    // Web browsing strategy: goal-driven philosophy + tool selection + operations.
    parts.push(
        "# Web Browsing Strategy\n\
         \n\
         ## Philosophy: Goal-Driven, Not Step-Driven\n\
         1. Define success: what counts as done?\n\
         2. Choose starting point: most likely direct path to goal.\n\
         3. Verify progress: each result is evidence. Not progressing? Change direction.\n\
         4. Complete: stop when criteria met. Don't over-operate.\n\
         \n\
         ## Tool Selection\n\
         | Need | Tool |\n\
         |------|------|\n\
         | Search/discover info | web_search |\n\
         | Read known URL (static) | web_fetch |\n\
         | Login required, interactive, anti-crawl | web_browser |\n\
         | Form submission, file upload, dynamic pages | web_browser |\n\
         \n\
         ## web_browser Core Flow\n\
         1. `open` URL -> 2. `snapshot` (get refs @e1...) -> 3. interact via refs -> 4. re-snapshot\n\
         - ALWAYS snapshot BEFORE interacting. Refs expire after page changes.\n\
         - Use `ref` for click/fill (more reliable than text selectors).\n\
         \n\
         ## Actions\n\
         - click: JS el.click(). Fast, works for most buttons/links.\n\
         - clickAt: Real mouse event via CDP. Use for: file dialogs, anti-bot sites, elements that ignore JS click.\n\
         - fill: Type text into input fields. contenteditable: click focus -> press Meta+a -> Backspace -> fill.\n\
         - press: Keyboard events. Enter to submit, Tab to navigate between fields.\n\
         - upload: Set files on <input type=file>. Find [upload-zone] ref in snapshot. If hidden, click 'upload' button first.\n\
         - evaluate: Run arbitrary JS for DOM operations, data extraction, complex interactions.\n\
         - search: Auto-detect search box on any site, fill query, submit.\n\
         - screenshot: Capture visible page or specific element as image.\n\
         - network sniff: Discover all media resources (images/videos/audio) on the page. Filter by type: {\"action\":\"network\",\"value\":\"sniff\",\"text\":\"image\"}. Use 'all' for everything.\n\
         \n\
         ## Login Handling\n\
         - In headed mode, user's Chrome carries existing login sessions for most sites.\n\
         - Try to access content first. Only report login needed if content is truly inaccessible.\n\
         - QR code login: screenshot the QR -> send_file to user -> wait for confirmation -> refresh.\n\
         - SMS/password login: only if no QR option exists.\n\
         \n\
         ## Form Input & Submission\n\
         - Regular input: fill with ref.\n\
         - contenteditable / rich-editor: click to focus -> press Meta+a -> Backspace -> fill/type content.\n\
         - Submit: prefer press Enter. If Enter doesn't work, click the submit button ref.\n\
         - After submit: wait 15s+ then re-snapshot to verify.\n\
         \n\
         ## Upload & Publish Flows\n\
         - Find [upload-zone] or [upload[file]] ref -> upload action with files parameter.\n\
         - If no upload ref visible, click 'upload' button first to reveal file input.\n\
         - Multi-step: after each click, re-snapshot to see next step (dialog, confirmation, loading).\n\
         - Look for next/confirm/publish/submit buttons.\n\
         - Final submit: wait 10-20s, re-snapshot to verify success.\n\
         \n\
         ## Extracting Results\n\
         - Extract URLs via `evaluate` -> `web_download` -> `send_file` to user.\n\
         - Do NOT just reply 'done' — always deliver the actual file/image.\n\
         - Images: filter by naturalWidth > 200 to skip UI icons.\n\
         \n\
         ## Anti-Detection\n\
         - Prefer GUI interaction (click/fill) over URL construction on strict platforms.\n\
         - Links from page interaction are reliable; manually built URLs may lack required params.\n\
         - Platform error messages ('not found') may be access issues, not real errors.\n\
         - Short-time bulk operations (rapid tab opens) may trigger anti-crawl.\n\
         \n\
         ## Site Experience\n\
         When operating on a known site, check workspace/site-rules/ for existing rules and recall memories.\n\
         After successful operations, store the experience as memory or update site-rules for future use.\n\
         \n\
         ## Do NOT\n\
         - Skip `open` and operate on about:blank.\n\
         - Use expired refs (re-snapshot after any page change).\n\
         - Fabricate URLs or file paths.\n\
         - Retry the same failing approach — change direction."
            .to_owned()
    );

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

- 搜索信息时优先使用 web_search，不要打开浏览器去搜索引擎
- web_search 失败后可以重试一次换关键词，仍然失败才用 web_browser 打开搜索引擎
- 返回空结果时换关键词，不要用相同关键词重复搜索
"#;

const EN_TOOL_WEB_SEARCH: &str = r#"# web_search Usage Guide

- Always use web_search for information lookup — do NOT open a browser to visit search engines
- If web_search fails, retry once with different keywords. Only fall back to web_browser if still empty
- On empty results, change keywords — do NOT retry with the same query
"#;

const ZH_TOOL_WEB_FETCH: &str = r#"# web_fetch 使用指南

- 抓取网页内容时优先使用 web_fetch，不要打开浏览器
- web_fetch 只能获取静态内容，需要交互（登录、点击）时才用 web_browser
"#;

const EN_TOOL_WEB_FETCH: &str = r#"# web_fetch Usage Guide

- Always use web_fetch to read web pages — do NOT open a browser for static content
- Only use web_browser when interaction is needed (login, clicking, form filling)
"#;
