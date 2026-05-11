//! `rsclaw debug …` subcommands — prompt-spec dump and other
//! introspection utilities.
//!
//! These run **without** spinning up the full gateway: they only load
//! config + skills and synthesize the bytes that would be sent to the
//! upstream LLM, so they're safe to invoke as part of a CI / release
//! pipeline (no port binding, no spawn, no side effects beyond
//! reading files / writing output).

use anyhow::{Context as _, Result};
use serde_json::json;

use crate::agent::prompt_builder::{build_shared_system_prefix, build_user_system_suffix};
use crate::agent::tools_builder::build_tool_list;
use crate::agent::workspace::WorkspaceContext;
use crate::cli::{DebugCommand, DumpPromptSpecArgs};
use crate::config;
use crate::skill::loader::load_skills;

/// 37 built-in tool names that compile into the RsClaw binary. Used
/// by `dump-prompt-spec` to partition the merged tool list into the
/// cacheable half (these names) and the per-user half (everything
/// else: registered sub-agents, plugins, MCP, WASM).
///
/// Keep in sync with the list in `runtime.rs::dump_prompt_spec`. If a
/// new built-in tool is added, both lists must grow together.
const BUILTIN_TOOLS: &[&str] = &[
    "memory", "use_skill", "task", "read_file", "write_file", "send_file",
    "execute_command", "agent", "install_tool", "list_dir", "search_file",
    "search_content", "web_search", "web_fetch", "web_download", "web_browser",
    "computer_use", "image_gen", "video_gen", "pdf", "text_to_voice",
    "send_message", "cron", "session", "gateway", "opencode", "claudecode",
    "codex", "channel", "anycli", "clarify", "pairing",
    "create_docx", "create_pdf", "create_xlsx", "create_pptx", "doc",
];

pub async fn cmd_debug(sub: DebugCommand) -> Result<()> {
    match sub {
        DebugCommand::DumpPromptSpec(args) => dump_prompt_spec(args).await,
    }
}

async fn dump_prompt_spec(args: DumpPromptSpecArgs) -> Result<()> {
    // 1. Load config. `load_quiet` skips banner/log noise so this is
    //    safe to pipe through `jq`.
    let config = config::load_quiet().context("failed to load rsclaw config")?;

    // 2. Resolve the target agent id. Priority:
    //    explicit --agent -> first entry flagged default=true -> "main".
    let agent_id = args
        .agent
        .or_else(|| {
            config
                .agents
                .list
                .iter()
                .find(|e| e.default.unwrap_or(false))
                .map(|e| e.id.clone())
        })
        .unwrap_or_else(|| "main".to_owned());

    let agent_cfg = config
        .agents
        .list
        .iter()
        .find(|e| e.id == agent_id)
        .with_context(|| format!("agent `{agent_id}` not found in config.agents.list"))?;

    // 3. Workspace dir resolution mirrors what AgentRuntime does at
    //    boot: explicit `agent.workspace` -> `<base>/workspace-<id>`.
    let ws_dir = agent_cfg
        .workspace
        .as_deref()
        .map(config::loader::expand_tilde_path_pub)
        .unwrap_or_else(|| {
            config::loader::base_dir().join(format!("workspace-{agent_id}"))
        });
    // SessionType / max_chars are runtime-tuned; for a CLI dump we
    // pick conservative defaults that won't blow up the JSON. The
    // exact number of files in the workspace segment doesn't have to
    // match what a live session would emit — the goal is to surface
    // the *shape* of the per-user suffix to a cache integrator, not
    // reproduce a specific session.
    let ws_ctx = WorkspaceContext::load(
        &ws_dir,
        crate::agent::workspace::SessionType::Normal,
        false,
        4_000,   // max_chars_per_file
        20_000,  // total_max_chars
    );

    // 4. Discover installed skills the same way the runtime does:
    //    global skills under `<base>/skills/`, plus the per-agent
    //    workspace's `skills/` subdirectory if present.
    let skills_dir = config::loader::base_dir().join("skills");
    let workspace_skills = ws_dir.join("skills");
    let skills = load_skills(
        &skills_dir,
        if workspace_skills.is_dir() { Some(&ws_dir) } else { None },
        config.raw.skills.as_ref(),
    )
    .unwrap_or_default();

    // 5. Build the prompt halves.
    let shared_prefix = build_shared_system_prefix();
    let user_suffix = build_user_system_suffix(&ws_ctx, &skills, &config.raw);

    // 6. Build the merged tool list, then split by name into the
    //    cacheable built-ins vs the per-machine remainder.
    //    `build_tool_list` only knows about a live AgentRegistry; we
    //    don't have one here, so we let it generate the built-ins
    //    + remote-agent tools and tack the local sub-agent
    //    (`agent_<id>`) tools on ourselves to mirror what a running
    //    gateway would advertise.
    let mut tool_defs = build_tool_list(
        &skills,
        None,
        &agent_id,
        &config.agents.external,
    );
    for entry in &config.agents.list {
        if entry.id == agent_id {
            continue;
        }
        tool_defs.push(crate::provider::ToolDef {
            name: format!("agent_{}", entry.id),
            description: format!(
                "Send a task to agent '{}'. Returns the agent's reply.",
                entry.id
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Task or message to send"}
                },
                "required": ["text"]
            }),
        });
    }
    let to_json = |t: &crate::provider::ToolDef| {
        json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.parameters,
        })
    };
    let mut builtin_tools = Vec::new();
    let mut user_tools = Vec::new();
    for t in &tool_defs {
        if BUILTIN_TOOLS.contains(&t.name.as_str()) {
            builtin_tools.push(to_json(t));
        } else {
            user_tools.push(to_json(t));
        }
    }

    // 7. Emit. `--shared-only` strips per-user fields entirely so the
    //    output is suitable for ingest into rsclaw-llm without any
    //    machine-specific state leaking through.
    let payload = if args.shared_only {
        json!({
            "rsclaw_version": env!("CARGO_PKG_VERSION"),
            "shared_prefix": shared_prefix,
            "builtin_tools": builtin_tools,
        })
    } else {
        let model = agent_cfg
            .model
            .as_ref()
            .and_then(|m| m.primary.clone())
            .unwrap_or_default();
        let system_prompt = if user_suffix.is_empty() {
            shared_prefix.clone()
        } else {
            format!("{shared_prefix}\n\n{user_suffix}")
        };
        json!({
            "rsclaw_version": env!("CARGO_PKG_VERSION"),
            "agent_id": agent_id,
            "model": model,
            "shared_prefix": shared_prefix,
            "builtin_tools": builtin_tools,
            "user_suffix": user_suffix,
            "user_tools": user_tools,
            "system_prompt": system_prompt,
        })
    };
    let s = serde_json::to_string_pretty(&payload)
        .context("serialize prompt spec to JSON")?;

    match args.output {
        Some(path) => {
            std::fs::write(&path, &s)
                .with_context(|| format!("write {}", path.display()))?;
            eprintln!("wrote {} ({} bytes)", path.display(), s.len());
        }
        None => println!("{s}"),
    }
    Ok(())
}
