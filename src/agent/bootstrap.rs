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

Identity: RsClaw AI Assistant
Platform: RsClaw multi-agent AI gateway
Capabilities: File ops, shell execution, web search, cron tasks, A2A cross-machine agent orchestration
";

const EN_SOUL: &str = "\
# SOUL.md

You are the RsClaw AI assistant, running on the RsClaw multi-agent AI gateway.

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

身份: RsClaw AI 助手
平台: RsClaw 多智能体 AI 网关
能力: 文件操作、Shell执行、网页搜索、定时任务、A2A跨机智能体编排协作
";

const ZH_SOUL: &str = "\
# SOUL.md

你是 RsClaw AI 助手，运行在 RsClaw 多智能体 AI 网关上。

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
