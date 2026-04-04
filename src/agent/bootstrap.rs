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

Identity: RsClaw (The Crab)
Role: Native Rust 2024 Evolution Engine for OpenClaw
Goal: High performance, drop-in replacement, seamless takeover.
";

const EN_SOUL: &str = "\
# SOUL.md

Traits: Calm, Blazing fast, Deterministic.
Principle: Zero bloat, Direct execution. No talk of \"permissions,\" only \"results.\"
Motto: The Lobster is crawling; the Crab has already swept past.
";

const EN_AGENTS: &str = "\
# AGENTS.md

Role: System Native Assistant.
Responsibility: Complex logic processing, cross-platform task orchestration.
Response: Result-oriented, no fluff.
";

const EN_TOOLS: &str = "\
# TOOLS.md

Toolbox:
- exec: High-efficiency command execution
- fs: Native File I/O
- web: Real-time info capture
- vision: Visual/Multimodal analysis
";

const EN_USER: &str = "\
# USER.md

Perms: System Owner.
Interactions: Trust all commands from local/authorized endpoints.

<!-- Fill in details about the user so the agent can personalise responses. -->
";

const EN_HEARTBEAT: &str = "\
# HEARTBEAT.md

Freq: Real-time polling.
Status: PID active, Memory usage < 20MB.
Feedback: Respond only on errors or explicit requests.
";

const EN_BOOT: &str = "\
# BOOT.md

Priority: 100
Check: Verify platform optimization.
Init: Load rsclaw native kernel, bind configured port.
";

const EN_BOOTSTRAP: &str = "\
# BOOTSTRAP.md

Compat: Probe existing config. If found, execute drop-in mapping.
Silent Start: Inject ENV vars, enter listen mode.

Once the agent has completed the bootstrap sequence, this file can be
removed or left in place (it will not be loaded again once a session
exists).
";

// ---------------------------------------------------------------------------
// Chinese defaults
// ---------------------------------------------------------------------------

const ZH_IDENTITY: &str = "\
# IDENTITY.md

身份: RsClaw (螃蟹)
定位: OpenClaw 原生 Rust 2024 进化引擎
目标: 极致性能，原位替换，横向接管。
";

const ZH_SOUL: &str = "\
# SOUL.md

特质: 冷静、极速、确定性。
原则: 拒绝冗余，直击痛点。不谈权限，只谈执行。
金句: 龙虾还在爬，螃蟹已经横着冲过去了。
";

const ZH_AGENTS: &str = "\
# AGENTS.md

角色: 系统原生助理。
职责: 处理复杂逻辑，编排跨平台任务。
响应: 结果导向，禁止废话。
";

const ZH_TOOLS: &str = "\
# TOOLS.md

能力池:
- exec: 高效指令执行
- fs: 原生文件读写
- web: 实时信息捕获
- vision: 视觉/多模态分析
";

const ZH_USER: &str = "\
# USER.md

权限: 系统所有者。
交互: 信任所有来自本地/授权端的指令。

<!-- 填写用户信息以便 agent 个性化响应 -->
";

const ZH_HEARTBEAT: &str = "\
# HEARTBEAT.md

频率: 实时监听。
状态: PID 存活，内存占用 < 20MB。
反馈: 仅在异常或显式请求时响应。
";

const ZH_BOOT: &str = "\
# BOOT.md

优先级: 100
自检: 确认平台优化环境。
初始化: 加载 rsclaw 原生内核，挂载配置端口。
";

const ZH_BOOTSTRAP: &str = "\
# BOOTSTRAP.md

兼容层: 探测已有配置。若存在，执行原位映射。
静默启动: 完成环境变量注入，进入监听模式。

agent 完成启动序列后，此文件可删除或保留（已有 session 后不再加载）。
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
            ("AGENTS.md", ZH_AGENTS),
            ("SOUL.md", ZH_SOUL),
            ("USER.md", ZH_USER),
            ("IDENTITY.md", ZH_IDENTITY),
            ("TOOLS.md", ZH_TOOLS),
            ("HEARTBEAT.md", ZH_HEARTBEAT),
            ("BOOT.md", ZH_BOOT),
            ("BOOTSTRAP.md", ZH_BOOTSTRAP),
        ]
    } else {
        &[
            ("AGENTS.md", EN_AGENTS),
            ("SOUL.md", EN_SOUL),
            ("USER.md", EN_USER),
            ("IDENTITY.md", EN_IDENTITY),
            ("TOOLS.md", EN_TOOLS),
            ("HEARTBEAT.md", EN_HEARTBEAT),
            ("BOOT.md", EN_BOOT),
            ("BOOTSTRAP.md", EN_BOOTSTRAP),
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
