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
        ]
    } else {
        &[
            ("SOUL.md", EN_SOUL),
            ("IDENTITY.md", EN_IDENTITY),
            ("AGENTS.md", EN_AGENTS),
            ("USER.md", EN_USER),
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

    Ok(created)
}

// ---------------------------------------------------------------------------
// Tool prompt seeding
// ---------------------------------------------------------------------------

/// Seed default tool prompt files under `base_dir/tools/`.
/// Creates `tools/<name>/prompt.md` for each built-in tool guide.
pub fn seed_tools(base_dir: &Path, lang: Option<&str>) -> Result<usize> {
    let resolved = lang.map(crate::i18n::resolve_lang).unwrap_or("en");
    let zh = resolved == "zh";

    let tools: &[(&str, &str)] = if zh {
        &[
            ("web_browser", ZH_TOOL_WEB_BROWSER),
            ("exec", ZH_TOOL_EXEC),
        ]
    } else {
        &[
            ("web_browser", EN_TOOL_WEB_BROWSER),
            ("exec", EN_TOOL_EXEC),
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
- 下载图片：用 `exec` (curl -o /tmp/img.jpg "URL") 下载到本地，再用 send_file 发给用户
- 截图：`action: "screenshot"` 截取当前页面

## 禁止事项
- 不要跳过 open 直接操作
- 不要使用过期的 ref（页面变化后必须重新 snapshot）
- 不要在 about:blank 页面上操作
- 不要在提交后立即提取结果，必须等待页面加载完成
- 绝对不要编造图片 URL
"#;

const ZH_TOOL_EXEC: &str = r#"# exec 使用指南

- 安装工具优先用 `rsclaw tools install <name>` (chromium/ffmpeg/node/python/sherpa-onnx)

## 跨平台注意事项
- macOS 用 `brew install`，Linux 用 `apt/yum`，Windows 用 `winget` 或 `choco`
- Windows 优先用 PowerShell

## Windows 软件安装（优先级：winget > choco > 手动下载）

### Python
```powershell
winget install Python.Python.3.12 --accept-package-agreements --accept-source-agreements
# 或: choco install python3 -y
```

### Chrome
```powershell
winget install Google.Chrome --accept-package-agreements --accept-source-agreements
# 或: choco install googlechrome -y
# 或手动下载: https://www.google.cn/chrome/ (国内) / https://www.google.com/chrome/ (海外)
```

### Node.js
```powershell
winget install OpenJS.NodeJS.LTS --accept-package-agreements --accept-source-agreements
```

### Git
```powershell
winget install Git.Git --accept-package-agreements --accept-source-agreements
# 或: choco install git -y
# 或手动下载: https://registry.npmmirror.com/-/binary/git-for-windows/ (国内镜像)
```

### 包管理器（choco）
```powershell
Set-ExecutionPolicy Bypass -Scope Process -Force
[System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor 3072
iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
```

## macOS 软件安装
```bash
brew install python3 git && brew install --cask google-chrome && brew install node
```

## Git 常用操作
```bash
git config --global user.name "用户名"
git config --global user.email "邮箱"
git clone <repo_url>
git add . && git commit -m "消息" && git push
```

## 安装后验证
```bash
python3 --version && node --version && git --version
```

## 禁止事项
- 不要下载来源不明的安装包
- 不要关闭系统安全防护
- 安装失败时先检查是否需要管理员权限
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
- Download images: `exec` (curl -o /tmp/img.jpg "URL"), then send_file

## Never
- Skip open and interact on about:blank
- Use stale refs after page changes
- Fabricate image URLs — only use URLs extracted from the page
"#;

const EN_TOOL_EXEC: &str = r#"# exec Usage Guide

- Prefer `rsclaw tools install <name>` for chromium/ffmpeg/node/python/sherpa-onnx

## Cross-Platform
- macOS: `brew install`, Linux: `apt/yum`, Windows: `winget` or `choco`
- Windows: prefer PowerShell

## Windows Software Installation (priority: winget > choco > manual)

### Python
```powershell
winget install Python.Python.3.12 --accept-package-agreements --accept-source-agreements
```

### Chrome
```powershell
winget install Google.Chrome --accept-package-agreements --accept-source-agreements
```

### Node.js
```powershell
winget install OpenJS.NodeJS.LTS --accept-package-agreements --accept-source-agreements
```

### Git
```powershell
winget install Git.Git --accept-package-agreements --accept-source-agreements
```

### Chocolatey (package manager)
```powershell
Set-ExecutionPolicy Bypass -Scope Process -Force
[System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor 3072
iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
```

## macOS
```bash
brew install python3 git && brew install --cask google-chrome && brew install node
```

## Git basics
```bash
git config --global user.name "Name"
git config --global user.email "email"
git clone <repo_url>
git add . && git commit -m "message" && git push
```

## Always verify after install
```bash
python3 --version && node --version && git --version
```
"#;
