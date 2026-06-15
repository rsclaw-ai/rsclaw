//! Tool list builder — generates the consolidated ToolDef list for an agent.
//!
//! Extracted from `runtime.rs` to reduce file size.
//! All public items are re-exported by `runtime.rs` so callers are unaffected.

use serde_json::{Value, json};

use super::registry::AgentRegistry;
use rsclaw_config::schema::A2aPeerConfig;
use rsclaw_plugin::{PluginRegistry, WasmPlugin};
use rsclaw_provider::ToolDef;
use rsclaw_skill::SkillRegistry;

pub(crate) fn build_plugin_meta_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "plugin_list".to_owned(),
            description: "Read-only catalog of installed plugins and their common tools. Use this when the user asks what plugins are available or when you need plugin overview before choosing plugin_search, plugin_describe, or plugin_invoke. This tool does not execute plugin actions.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string", "description": "Optional installed plugin name, e.g. douyin. Omit to list all installed plugins."}
                }
            }),
        },
        ToolDef {
            name: "plugin_search".to_owned(),
            description: "Search or browse installed plugin tool catalogs. With non-empty `query`: ranks tools by intent match (use before plugin_invoke when you need a capability but don't know the exact tool name). With empty `query` and `plugin` set: lists ALL tools in that plugin alphabetically (paginate via `offset`/`limit`).".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string", "description": "Optional installed plugin name, e.g. douyin. Omit to search all plugins (requires non-empty query)."},
                    "query": {"type": "string", "description": "Short user intent, e.g. 'publish video'. Empty/omitted is allowed only when `plugin` is given — then returns the alphabetical full tool list."},
                    "limit": {"type": "integer", "description": "Maximum tools to return. Default 8, cap 50."},
                    "offset": {"type": "integer", "description": "Pagination offset, default 0. Use with the returned `next_offset` to walk a long plugin."}
                }
            }),
        },
        ToolDef {
            name: "plugin_describe".to_owned(),
            description: "Return the full manifest description and input schema for one installed plugin tool. Use after plugin_search when you need exact argument details.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string", "description": "Installed plugin name, e.g. douyin."},
                    "tool": {"type": "string", "description": "Tool name inside the plugin, without plugin prefix, e.g. publish."}
                },
                "required": ["plugin", "tool"]
            }),
        },
        ToolDef {
            name: "plugin_invoke".to_owned(),
            description: "Call an installed plugin tool after discovering it with plugin_search or plugin_describe. The host validates the tool exists and checks required arguments against the plugin manifest before dispatch.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin": {"type": "string", "description": "Installed plugin name, e.g. douyin."},
                    "tool": {"type": "string", "description": "Tool name inside the plugin, without plugin prefix, e.g. publish."},
                    "arguments": {"type": "object", "description": "Arguments for the plugin tool. Must match the schema returned by plugin_search or plugin_describe."}
                },
                "required": ["plugin", "tool", "arguments"]
            }),
        },
    ]
}

/// Render a plugin's tools as `- name: one-line purpose` lines for the catalog.
/// Tools named in `common` come first (in `common` order); the rest follow
/// sorted by name. Output is capped at `cap`; the remainder is summarised with
/// a pointer to plugin_search. Byte-stable for a given input (KV-cache
/// hygiene in the per-machine layer). `tools` is (name, description) pairs.
fn plugin_tool_list(tools: &[(&str, &str)], common: &[String], cap: usize) -> String {
    if tools.is_empty() {
        return "  (no declared tools)".to_owned();
    }
    let is_common = |n: &str| common.iter().any(|c| c == n);
    // Common first, in declared order (only those that actually exist).
    let mut ordered: Vec<(&str, &str)> = common
        .iter()
        .filter_map(|c| tools.iter().find(|(n, _)| n == c).copied())
        .collect();
    // Then the rest, sorted by name.
    let mut rest: Vec<(&str, &str)> = tools
        .iter()
        .filter(|(n, _)| !is_common(n))
        .copied()
        .collect();
    rest.sort_unstable_by(|a, b| a.0.cmp(b.0));
    ordered.extend(rest);

    let total = ordered.len();
    let mut lines: Vec<String> = ordered
        .iter()
        .take(cap)
        .map(|(n, d)| {
            let d = d.trim();
            if d.is_empty() {
                format!("- {n}")
            } else {
                let short: String = d.chars().take(100).collect();
                format!("- {n}: {}", short.trim_end())
            }
        })
        .collect();
    if total > cap {
        let n = total - cap;
        let noun = if n == 1 { "tool" } else { "tools" };
        lines.push(format!(
            "- …{n} more {noun} — plugin_search to find them"
        ));
    }
    lines.join("\n")
}

/// Render one plugin's catalog block, SKILL.md-style: heading with summary,
/// then its common tools. `summary_or_desc` is the manifest summary (falls back
/// to description upstream). `tools` is (name, description) pairs.
fn render_plugin_catalog_block(
    name: &str,
    version: &str,
    summary_or_desc: &str,
    tools: &[(&str, &str)],
    common: &[String],
    cap: usize,
) -> String {
    let list = plugin_tool_list(tools, common, cap);
    // Omit the " — <blurb>" when neither summary nor description is set, so the
    // heading doesn't render a dangling em-dash.
    let heading = if summary_or_desc.is_empty() {
        format!("### {name} (v{version})")
    } else {
        format!("### {name} — {summary_or_desc} (v{version})")
    };
    format!(
        "{heading}\n\
         Common tools (call via plugin_invoke {{plugin:\"{name}\", tool, arguments}}; \
         use plugin_search {{plugin:\"{name}\", query}} for others):\n\
         {list}"
    )
}

/// Build a system-prompt section that lists installed plugins (wasm + shell).
/// Helps the model decide *to use* the plugin instead of falling back to a
/// generic browser-automation flow. Sorted by name for byte-stable output.
pub(crate) fn build_plugins_system(
    wasm_plugins: &[WasmPlugin],
    js_plugins: Option<&PluginRegistry>,
) -> Option<String> {
    let no_js = js_plugins
        .map(|r| r.js_plugins_iter().next().is_none())
        .unwrap_or(true);
    if wasm_plugins.is_empty() && no_js {
        return None;
    }

    let mut blocks: Vec<(String, String)> = wasm_plugins
        .iter()
        .map(|p| {
            let tools: Vec<(&str, &str)> = p
                .tools
                .iter()
                .map(|t| (t.name.as_str(), t.description.as_str()))
                .collect();
            let blurb = p
                .summary
                .as_deref()
                .or(p.description.as_deref())
                .unwrap_or("");
            (
                p.name.clone(),
                render_plugin_catalog_block(
                    &p.name,
                    p.version.as_deref().unwrap_or(""),
                    blurb,
                    &tools,
                    &p.common_tools,
                    8,
                ),
            )
        })
        .collect();

    if let Some(reg) = js_plugins {
        for (plugin_name, plugin) in reg.js_plugins_iter() {
            let m = &plugin.manifest;
            let tools: Vec<(&str, &str)> = m
                .tools
                .iter()
                .map(|t| (t.name.as_str(), t.description.as_str()))
                .collect();
            let blurb = m
                .summary
                .as_deref()
                .or(m.description.as_deref())
                .unwrap_or("");
            blocks.push((
                plugin_name.clone(),
                render_plugin_catalog_block(
                    plugin_name,
                    m.version.as_deref().unwrap_or(""),
                    blurb,
                    &tools,
                    &m.common_tools,
                    8,
                ),
            ));
        }
    }

    // Sort by name for byte-stable output (HashMap iteration order is
    // nondeterministic; this matters because the system prompt feeds the
    // LLM's KV cache, and unstable ordering invalidates the cache).
    blocks.sort_by(|a, b| a.0.cmp(&b.0));
    let blocks_text: Vec<String> = blocks.into_iter().map(|(_, b)| b).collect();

    // Priority ordering (WASM plugins > JS plugins > skills > built-in tools) lives
    // in the shared `CAPABILITY PRIORITY` section of
    // `build_shared_system_prefix` so it ships unconditionally with the
    // version baseline. Repeating it here would (a) duplicate ~30 tokens
    // every time plugins are present and (b) make the rendered bytes
    // diverge from the version-pinned baseline in a way that could
    // disturb the rsclaw-llm static prefix cache assumptions.
    Some(format!(
        "## Installed Plugins\n\
         Each plugin bundles many tools. The common ones are listed below; \
         call them with `plugin_invoke`, and use `plugin_search` \
         {{plugin, query}} to find any not listed. Prefer a plugin over a \
         generic browser flow when it covers the task.\n\n\
         {}",
        blocks_text.join("\n\n"),
    ))
}

/// Builtin tools with near-zero observed production usage (tool-dispatch
/// frequency over the gateway log, 2026-06). On non-rsclaw providers these
/// are deferred behind the [`request_tool_def`] stub instead of paying their
/// full definitions (~2.8k tokens) on every request. rsclaw protocol is
/// exempt — its tools ride in the shared registry prefix, where the fleet's
/// KV cache makes them free at the margin.
///
/// Deliberately NOT in this list despite low usage: `web_download` (the
/// web_fetch/web_browser guides route downloads here by name), `channel` /
/// `send_message` (hub deployments use them even though desktop never does),
/// and anything plugin_* (separate discovery path).
pub(crate) const COLD_TOOLS: &[&str] = &[
    "doc",
    "cap_bind_sticky",
    "cap_unbind_sticky",
    "video_gen",
    "avatar_gen",
    "mv_gen",
    "audio_gen",
    "create_docx",
    "create_pdf",
    "create_pptx",
    "create_xlsx",
    "text_to_voice",
    "pairing",
    "gateway",
    "session",
    "skill_remove",
    "wait_input",
];

/// One-line stub standing in for everything deferred: builtin
/// [`COLD_TOOLS`] (entries are bare tool names) and plugin tool groups
/// (entries are `<plugin>:<group>` with a descriptive label). Calling it
/// enables the named tool/group for the rest of the session — the runtime
/// splices the real definitions back into the live tool list before the
/// next LLM iteration of the SAME turn, so the cost is one extra
/// round-trip on first use.
pub(crate) fn request_tool_def(entries: &[(String, String)]) -> ToolDef {
    let lines: Vec<String> = entries
        .iter()
        .map(|(key, label)| {
            if key == label {
                format!("- {key}")
            } else {
                format!("- {label}")
            }
        })
        .collect();
    ToolDef {
        name: "request_tool".to_owned(),
        description: format!(
            "Enable a deferred tool or tool group, then call the tool(s) directly. \
             These exist but their full definitions load on demand:\n{}",
            lines.join("\n")
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Tool name or <plugin>:<group> to enable"}
            },
            "required": ["name"]
        }),
    }
}

/// Compute the set of allowed tool names based on toolset level + custom tools.
/// Returns None for "full" (no filtering), Some(set) for others.
pub fn toolset_allowed_names(
    toolset: &str,
    custom_tools: Option<&Vec<String>>,
) -> Option<std::collections::HashSet<String>> {
    const MINIMAL: &[&str] = &[
        "shell",
        "read_file",
        "write_file",
        "send_file",
        "list_dir",
        "search_file",
        "search_content",
        "web_search",
        "web_fetch",
        "memory",
        "clarify",
        "anycli",
        "skill_use",
        "plugin_list",
        "plugin_search",
        "plugin_describe",
        "plugin_invoke",
    ];
    const WEB: &[&str] = &[
        "plugin_list",
        "plugin_search",
        "plugin_describe",
        "plugin_invoke",
        "web_search",
        "web_fetch",
        "web_download",
        "read_file",
        "write_file",
        "list_dir",
        "search_file",
        "memory",
        "todo",
        "skill_use",
    ];
    const CODE: &[&str] = &[
        // Plugin discovery + invoke
        "plugin_list",
        "plugin_search",
        "plugin_describe",
        "plugin_invoke",
        // Core file ops
        "shell",
        "read_file",
        "write_file",
        "edit_file",         // exact-string replace; cheaper than rewriting whole files
        "list_dir",
        "search_file",
        "search_content",
        // Large tool output handling: backstop artifact (tr_xxxxxxxx) reader
        "read_artifact",
        // Memory / KB / skills (read-mostly; users still want recall here)
        "memory",
        "knowledge_base",
        "skill_use",
        // Working-plan checklist — multi-step coding tasks are exactly where
        // plan tracking pays off (and where compaction loses implicit plans).
        "todo",
        // Pre-compaction history search (multi-turn coding sessions hit this often)
        "read_session_archive",
        // Coding-agent escalation — dispatch big tasks to an external CLI
        // coding agent (claude-code / openclaude / opencode / codex) via cap-rs.
        "cap",
        // Interactive cap session — multi-turn warm session against a single
        // agent, useful for orchestrating several cap agents in one IM turn.
        "cap_live",
        "cap_live_end",
        // Sticky direct-mode bind/unbind — the user can ask in natural
        // language ("接下来都让 claudecode 来" / "back to normal") and the
        // LLM translates to these tools. After bind, the runtime routes
        // subsequent user messages straight to the cap driver, skipping
        // the LLM entirely until unbind.
        "cap_bind_sticky",
        "cap_unbind_sticky",
        // Sub-agent dispatch — rsclaw-native task/spawn for non-coding sub-work
        "agent",
        // Disambiguation: ask the user when the task description is genuinely ambiguous
        "clarify",
        // Mark task done / send final report
        "task",
    ];
    const STANDARD: &[&str] = &[
        "shell",
        "read_file",
        "write_file",
        "list_dir",
        "search_file",
        "search_content",
        "web_search",
        "web_fetch",
        "memory",
        "todo",
        "web_browser",
        "image_gen",
        "ocr",
        "video_gen",
        "avatar_gen",
        "mv_gen",
        "audio_gen",
        "channel",
        "cron",
        "computer_use",
        "clarify",
        "anycli",
        "skill_use",
        "skill_list",
        "skill_search",
        "skill_install",
        "skill_remove",
        "task",
        "plugin_list",
        "plugin_search",
        "plugin_describe",
        "plugin_invoke",
        // astock — built-in tools, dormant when not configured.
        "stock_quote",
        "stock_kline",
        "stock_snapshot",
        "stock_ask",
        "stock_query",
        "stock_chart",
        "stock_watchlist",
        "research_ingest_wechat",
        "research_analyze_charts",
    ];

    let base: Option<&[&str]> = match toolset {
        "minimal" => Some(MINIMAL),
        "web" => Some(WEB),
        "code" => Some(CODE),
        "standard" => Some(STANDARD),
        "full" => None,
        _ => Some(STANDARD),
    };

    // Semantics: `toolset` is the preset (used when nothing more specific is
    // set). `tools` is the explicit specification (overrides the preset).
    // Previously the two were merged — which broke hub-router agents that
    // set `toolset: minimal` + `tools: [agent_spoke_*]` expecting only the
    // peers but actually got minimal's shell + read_file + ... too, and 9B
    // models then narrated `pip install` instead of routing. Override is the
    // intuitive contract.
    match (base, custom_tools) {
        (None, None) => None, // full, no custom -> no filtering
        (_, Some(extra)) => {
            // tools specified — exclusive whitelist, regardless of toolset.
            Some(extra.iter().cloned().collect())
        }
        (Some(base_list), None) => Some(base_list.iter().map(|s| s.to_string()).collect()),
    }
}

/// Build the complete tool list for an agent runtime.
///
/// Includes built-in tools, per-agent A2A tools, external agent tools,
/// and skill-derived tools.
pub fn build_tool_list(
    skills: &SkillRegistry,
    agents: Option<&AgentRegistry>,
    caller_id: &str,
    a2a_peers: &[A2aPeerConfig],
) -> Vec<ToolDef> {
    let mut tools = Vec::new();

    // Built-in tools — consolidated (32+ tools -> ~13 unified tools).
    tools.push(ToolDef {
        name: "memory".to_owned(),
        description: "Manage long-term memory across sessions.\n\
            Actions:\n\
            - search: Semantic search over stored memories. Example: {\"action\":\"search\",\"query\":\"user preferences\"}\n\
            - get: Retrieve a specific memory by ID. Example: {\"action\":\"get\",\"id\":\"abc-123\"}\n\
            - put: Store a new memory. Example: {\"action\":\"put\",\"text\":\"User prefers dark mode\",\"kind\":\"fact\"}\n\
            Use this tool to recall prior context, user preferences, or previously learned information.\n\
            Search BEFORE answering questions about past conversations or user details.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["search", "get", "put"], "description": "search | get | put"},
                "query":  {"type": "string", "description": "Search query (search)."},
                "id":     {"type": "string", "description": "Memory id (get)."},
                "text":   {"type": "string", "description": "Content to store (put); be specific, include context."},
                "scope":  {"type": "string", "description": "Optional scope filter."},
                "kind":   {"type": "string", "description": "note | fact | remember. Never summary (auto-written by /compact, /new)."},
                "top_k":  {"type": "integer", "default": 5, "description": "Max results (search)."}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "todo".to_owned(),
        description: "Maintain this session's working plan as a status checklist. \
            FULL-REPLACE semantics: every call must send the COMPLETE updated list \
            (include finished items with status done); an empty items array clears the plan.\n\
            When to use: any task needing 3+ steps or multiple tool calls — write the plan \
            FIRST, keep exactly ONE item in_progress, flip it to done IMMEDIATELY after the \
            step lands, then promote the next. Skip it for single-step requests and chat.\n\
            The plan is persisted outside the conversation: it survives context compaction, \
            so an accurate list is your recovery anchor in long sessions.\n\
            Example: {\"items\":[{\"text\":\"Read the config loader\",\"status\":\"done\"},\
            {\"text\":\"Add the provider flag\",\"status\":\"in_progress\"},\
            {\"text\":\"Run cargo test\",\"status\":\"pending\"}]}"
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "description": "The complete plan, in execution order (full replace). Empty array clears it.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text":   {"type": "string", "description": "One concrete step."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "done"], "description": "Defaults to pending."}
                        },
                        "required": ["text"]
                    }
                }
            },
            "required": ["items"]
        }),
    });
    tools.extend(build_plugin_meta_tool_defs());
    // `skill_use` — first-class entry point for installed skills. Listed
    // EARLY in the tool list so the LLM notices it before web_fetch /
    // web_browser / shell. Registered UNCONDITIONALLY (even with zero
    // skills installed) so the cacheable base-layer prefix — and the
    // exported baseline that seeds the rsclaw-server prefix slot — is
    // DETERMINISTIC regardless of the generating host's skill set. With no
    // skills installed the model just has nothing valid to pass; that's
    // cheaper than a baseline whose tool set silently depends on whether
    // the dump machine happened to have a skill on disk.
    //
    // Critical invariant: this description does NOT enumerate installed
    // skill names. The skill list lives in `user_system` (rendered by
    // `build_user_system` as "## Installed Skills"), which the worker
    // treats as a per-session layer that does NOT participate in the
    // base layer hash. Embedding skill names HERE — inside `tools` —
    // would put them in the base-layer hash input and break cross-host
    // cache reuse for clients with different skill sets installed.
    tools.push(ToolDef {
        name: "skill_use".to_owned(),
        description:
            "ACTIVATE an installed skill. Use this BEFORE web_fetch / web_browser / \
            shell whenever the user's task matches any skill description \
            shown in the system prompt under '## Installed Skills' (flights, hotels, \
            stocks, weather, finance data, etc.).\n\n\
            Returns the full SKILL.md so you know the exact CLI command and flags. \
            After calling skill_use you typically call shell with the CLI \
            from skill_md.\n\n\
            Common failure to avoid: defaulting to web_fetch on a domain a skill \
            already covers. If a skill description matches, you MUST skill_use first. \
            If NO installed skill matches but one likely exists (e.g. restaurants → \
            meituan, stocks → hithink), `skill_search` for it, `skill_install` it, \
            then skill_use it.\n\n\
            Anti-loop guard: if you can already see the skill's content rendered in \
            the current turn (returned by a previous skill_use call), DO NOT call \
            skill_use again — follow the instructions directly."
                .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact skill name from the '## Installed Skills' list (e.g. 'flyai', 'hithink-market-query'). Case-sensitive."
                }
            },
            "required": ["name"]
        }),
    });
    tools.push(ToolDef {
        name: "skill_list".to_owned(),
        description: "List installed local skills with pagination. Use query first to filter by skill name/description, then use limit/offset for more results. Do not use shell to run `rsclaw skills list`; this tool is the authoritative installed-skill listing.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Optional case-insensitive filter matched against skill name and description."},
                "limit": {"type": "integer", "description": "Maximum skills to return. Default 50, maximum 100."},
                "offset": {"type": "integer", "description": "Zero-based offset for pagination."}
            }
        }),
    });
    tools.push(ToolDef {
        name: "skill_search".to_owned(),
        description: "Search the remote skill registries for an installable skill when no \
            installed skill matches the task and web tools won't cut it (e.g. \
            'restaurant'→meituan, 'stock quote'→hithink). Returns candidate slugs; \
            install one with skill_install then activate with skill_use."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What capability you need, e.g. 'restaurant booking', 'stock market data'."}
            },
            "required": ["query"]
        }),
    });
    tools.push(ToolDef {
        name: "skill_install".to_owned(),
        description: "Install a skill by slug (from skill_search results) into the local \
            skills store. Usable immediately via skill_use. Only install skills the \
            user's task clearly needs. Audited skills install directly; if a skill \
            is NOT on the audited allowlist the call returns needs_confirmation — ask \
            the user whether they trust it, and only on a clear yes retry with \
            confirmed=true. Never set confirmed=true without the user's explicit ok."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill slug to install (from skill_search)."},
                "confirmed": {"type": "boolean", "description": "Set true ONLY after the user explicitly approved installing an off-allowlist (unaudited) skill. Ignored for audited skills."}
            },
            "required": ["name"]
        }),
    });
    tools.push(ToolDef {
        name: "skill_remove".to_owned(),
        description: "Uninstall a locally-installed skill by name. Use only when the user \
            asks to remove it."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Installed skill name to remove."}
            },
            "required": ["name"]
        }),
    });
    // `task` — escalate the current chat into a multi-turn background task.
    // The LLM decides when sustained work is warranted (implementation
    // spanning many tool calls, multi-file refactor, deep research). For
    // short Q&A, jokes, greetings, and one-shot tool calls the LLM should
    // just answer directly without calling this tool.
    tools.push(ToolDef {
        name: "task".to_owned(),
        description: "Escalate the user's current request into a multi-turn background task. \
            Call this ONLY when the work clearly needs sustained execution: implementation \
            across multiple files, multi-step debugging, deep research with many web fetches, \
            data pipelines, end-to-end deployments. \
            Do NOT call for: greetings, casual questions, single tool calls (one web_search, \
            one read_file, one calculation), explanations, or anything you can answer in this \
            same turn. \
            Do NOT use this to dispatch to an external CLI coding agent (claude / claudecode / \
            openclaude / opencode / codex) — those need `cap`, not `task`. \
            When in doubt, just answer directly — the user can always send \
            `/task <request>` to escalate manually.\n\n\
            Returns a task ID; the gateway then runs the work in the background and posts \
            replies as turns complete. After calling task, your reply to the user should be a \
            short acknowledgement only — the actual work happens in the background turns.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "task_text": {
                    "type": "string",
                    "description": "The task instruction for the background runner. Usually the user's original request, optionally clarified."
                },
                "max_turns": {
                    "type": "integer",
                    "description": "Optional cap on agent turns. Default 10. Raise for big jobs (e.g. 30 for full feature implementation)."
                },
                "ttl_secs": {
                    "type": "integer",
                    "description": "Optional wall-clock deadline in seconds. Default 3600 (1h)."
                }
            },
            "required": ["task_text"]
        }),
    });
    // task_finish — agent-declared structured outcome. Replaces fragile
    // string-matching in the auto-continue supervisor by letting the agent
    // self-report what it did.
    tools.push(ToolDef {
        name: "task_finish".to_owned(),
        description: "Declare your task complete (or deliberately abandoned) and report a \
            structured outcome to the orchestrator. Call this as your LAST tool of the turn \
            when you believe the work is done — do not emit additional narrative text after \
            it. Failing to call task_finish leaves the orchestrator guessing from your prose \
            and can trigger a spurious auto-continue.\n\
            \n\
            When to call:\n\
            - You finished the user's ask                → completion=full,    recommend=ship\n\
            - You did a useful subset, more work obvious → completion=partial, recommend=continue, populate follow_up_tasks\n\
            - You're blocked and need the user           → completion=partial|minimal, recommend=needs_human, populate blocked_on\n\
            - Transient failure, retry might work        → recommend=retry\n\
            - You give up                                → completion=failed,  recommend=abandon, explain in blocked_on\n\
            \n\
            Honesty contract (the orchestrator may verify):\n\
            - verified=true REQUIRES verification_log with actual command + output excerpt.\n\
              \"I read the code and it looks right\" is NOT verification.\n\
            - accomplished entries must map to observable artifacts (file changed, command\n\
              run, message sent). Vague claims like 'improved the code' are not allowed.\n\
            - Asking the user a clarifying question is NOT completion=full — use\n\
              recommend=needs_human and put the question in blocked_on.\n\
            - completion=full with empty accomplished is rejected.\n\
            ".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "completion": {
                    "type": "string",
                    "enum": ["full", "partial", "minimal", "failed"],
                    "description": "How complete the work is. full=everything asked, partial=useful subset, minimal=almost nothing, failed=attempted but nothing landed."
                },
                "recommend": {
                    "type": "string",
                    "enum": ["ship", "continue", "needs_human", "retry", "abandon"],
                    "description": "What the orchestrator should do next. ship=deliver to user; continue=auto-spawn follow_up_tasks; needs_human=surface blocked_on to user; retry=same task fresh attempt; abandon=stop."
                },
                "verified": {
                    "type": "boolean",
                    "description": "True only if you actually ran tests/build/lint/curl and saw success. Lying is the worst possible move — orchestrator may verify."
                },
                "verification_log": {
                    "type": "string",
                    "description": "REQUIRED when verified=true. Short excerpt of the command + its output (e.g. 'cargo test → 47 passed, 0 failed')."
                },
                "accomplished": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Concrete things you did. Each entry should map to an observable artifact, e.g. 'Added enum variant TaskOutcome::Structured in task_queue.rs:64', 'Ran rg \"old_pattern\" — 0 matches confirms rename complete'."
                },
                "skipped": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "what": {"type": "string"},
                            "why":  {"type": "string"}
                        },
                        "required": ["what", "why"]
                    },
                    "description": "Things you deliberately skipped, with reasons. Explicit skips beat silent omissions."
                },
                "blocked_on": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Concrete unresolved blockers. When recommend=needs_human, each entry is surfaced verbatim to the user — phrase as questions or precise requests, not vague concerns."
                },
                "assumptions": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Assumptions you made about ambiguous spec. Non-empty + completion<full triggers orchestrator to confirm with user."
                },
                "follow_up_tasks": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Self-contained next-task descriptions (each will be spawned as its own task if recommend=continue). NOT 'TODO comments' — must be actionable."
                },
                "summary": {
                    "type": "string",
                    "description": "Optional one-paragraph prose summary for the channel reply."
                }
            },
            "required": ["completion", "recommend"]
        }),
    });
    tools.push(ToolDef {
        name: "read_file".to_owned(),
        description: "Read a file from the agent workspace.\n\
            Path is relative to workspace root.\n\
            Supports text files, code, config, markdown; .pdf / .docx / .xlsx / .pptx \
            are auto-extracted to plain text.\n\
            Example: {\"path\":\"config.json\"} or {\"path\":\"src/main.py\"}\n\
            \n\
            For text files, output is paginated. By default the first 2000 lines are \
            returned. Use `offset` (1-indexed line) and `limit` (max lines) for large \
            files. When the result is truncated, the response contains \
            `truncated: true` and `next_offset` — continue with `offset=next_offset` \
            until you have what you need. If you only need to scan for a substring, \
            `search_content` is cheaper than paging an entire file.\n\
            \n\
            For very large outputs (>~4 KB) the runtime backstop replaces the inline \
            content with a `tool_result_id` (tr_xxxxxxxx) — call `read_artifact` with \
            `mode=grep:PATTERN` / `head:N` / `tail:N` / `lines:A-B` instead of paging \
            via offset, it's much cheaper than re-reading via offset.\n\
            \n\
            If you just edited the file in this turn, you do not need to read it again — \
            the prior `read_file` result still reflects what you wrote. Re-reading on \
            every edit wastes a turn.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":   {"type": "string", "description": "Relative file path, e.g. 'src/app.py'."},
                "offset": {"type": "integer", "default": 1, "description": "1-indexed start line."},
                "limit":  {"type": "integer", "default": 2000, "description": "Max lines."}
            },
            "required": ["path"]
        }),
    });
    tools.push(ToolDef {
        name: "read_session_archive".to_owned(),
        description: "Search the FULL pre-compaction conversation history of the current session.\n\
            \n\
            When a session is compacted, you receive `[head + summary + recent]` in context, \
            but every original message is still on disk (redb archive table, never deleted). \
            Use this tool when the summary lacks specifics you need — e.g. the user asks \
            about something discussed earlier and the summary doesn't have the verbatim detail.\n\
            \n\
            Modes:\n\
              - mode=\"stat\"      — totals + seq range + generations, no content (CHEAP — check first)\n\
              - mode=\"head:N\"    — first N archived messages (oldest)\n\
              - mode=\"tail:N\"    — last N archived messages (newest)\n\
              - mode=\"seq:A-B\"   — inclusive 1-indexed seq range, e.g. \"seq:120-140\"\n\
              - mode=\"grep:PAT\"  — case-insensitive substring or alternation, e.g. \"grep:error|warn\" or \"grep:user paid\"\n\
            \n\
            Strategy: PREFER grep when you know a keyword — cheapest way to scan thousands of \
            messages. Use stat first if the session is huge and you want to estimate cost. \
            Use tail when the user says \"just earlier\" / \"刚刚\". Use seq:A-B for targeted \
            scrolling around a known seq. Don't pull head/tail with large N; use grep.\n\
            \n\
            Large messages get nested through the artifact pipeline — if a result row has \
            `tool_result_id`, call read_artifact for its full content. Grep results are \
            capped at 50 hits per call; tighten the pattern if more.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "description": "stat | head:N | tail:N | seq:A-B | grep:PATTERN. Default `stat`."},
                "generation": {"type": "integer", "description": "Optional — restrict to a specific session generation (post-/clear). Omit for all."}
            }
        }),
    });
    tools.push(ToolDef {
        name: "read_artifact".to_owned(),
        description: "Read the content of a tool_result artifact written by the runtime backstop.\n\
            \n\
            When any tool produces output larger than ~4 KB, the runtime writes the full \
            payload to disk and shows you only a head+tail preview in the tool_result. The \
            preview ends with `... call read_artifact(tool_result_id=\"tr_xxxxxxxx\") ...`. \
            That id is your handle.\n\
            \n\
            DEFAULT (no mode): returns the NEXT unread chunk and advances a server-side \
            cursor. To read a long artifact, just call read_artifact again (no mode) until \
            you see `at_end` — you do NOT compute line ranges, and you cannot accidentally \
            re-read the same page. Content that fits in one page comes back whole on the \
            first call. If the artifact is large, the first chunk is prefixed with an AI \
            summary so you can decide whether to keep paging or jump straight to specifics.\n\
            \n\
            Modes (optional):\n\
              - (none)             next unread chunk (cursor advances) — the normal way to read\n\
              - mode=\"reset\"       restart paging from the top\n\
              - mode=\"query:Q\"     SEMANTIC search — returns the sections most relevant to \
            question Q. BEST way to find a fact in a big artifact; prefer over paging when \
            you know what you're looking for.\n\
              - mode=\"grep:PAT\"    case-insensitive regex over lines (exact-string match; alternation `a|b|c` works)\n\
              - mode=\"lines:A-B\"   inclusive 1-indexed line range\n\
              - mode=\"head:N\" / \"tail:N\"   first / last N lines\n\
              - mode=\"stat\"        size + line count only, no content (CHEAP)\n\
            \n\
            Artifacts are session-scoped — an id from another session won't resolve. They \
            also expire after 7 days.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "tool_result_id": {"type": "string", "description": "Artifact id of the form `tr_xxxxxxxx` returned in a prior compacted tool_result."},
                "mode": {"type": "string", "description": "Omit to read the next chunk (cursor advances — the normal way). Or: query:QUESTION (semantic search) | grep:PATTERN | lines:A-B | head:N | tail:N | reset | stat."}
            },
            "required": ["tool_result_id"]
        }),
    });
    tools.push(ToolDef {
        name: "knowledge_base".to_owned(),
        description: "Search the user's KNOWLEDGE BASE — documents, PDFs, web pages and \
            files the user explicitly added for reference. Returns the most relevant \
            chunks WITH their source title, so you can cite where an answer came from.\n\
            \n\
            Use this when the user asks about THEIR material (\"what does my contract \
            say\", \"per the spec I uploaded\", \"summarize the docs\"), or whenever a \
            factual answer should be grounded in and cited from their corpus.\n\
            \n\
            How it differs from other tools:\n\
            - `memory` = things YOU learned about the user/past sessions (self-authored, \
              may decay, no citation). The knowledge base is user-curated, authoritative, \
              and citable.\n\
            - `web_search` = the live public web. The knowledge base is the user's own \
              private/local material.\n\
            \n\
            Prefer the knowledge base over web_search when the question is about content \
            the user gave you. Always cite the returned `source_title`. If it returns no \
            hits, say so — do NOT fabricate a citation.\n\
            \n\
            Writing — ONLY when the user explicitly asks you to save/organize material into \
            their knowledge base (e.g. \"整理这些微信记录录入知识库\", meeting notes, emails). \
            Never curate proactively.\n\
            - action=list_collections — see existing collections (reuse one before creating).\n\
            - action=create_collection {name, description?} — make a new collection.\n\
            - action=add {collection, title, content, mime?, force?} — ingest text you prepared; \
              `collection` is a NAME (created if absent); `content` is markdown/plain text. \
              Returns `status:\"near_duplicate\"` with the existing doc if a semantically similar \
              entry already exists; pass `force:true` to add anyway.\n\
            - action=delete {collection, doc_id} — tombstone a doc by its exact id. Only call when \
              the user explicitly asks to remove something; never to clean up your own mistakes \
              without confirmation.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["search", "add", "delete", "create_collection", "list_collections"], "description": "Default search. Write actions only when asked."},
                "query": {"type": "string", "description": "Search query."},
                "collection_ids": {"type": "array", "items": {"type": "string"}, "description": "Restrict to these collection ids; omit for all."},
                "top_k": {"type": "integer", "default": 5, "description": "Max hits."},
                "collection": {"type": "string", "description": "Collection NAME or ID (add/delete)."},
                "name": {"type": "string", "description": "New collection name."},
                "description": {"type": "string", "description": "Optional collection description."},
                "title": {"type": "string", "description": "Document title."},
                "content": {"type": "string", "description": "Document text to ingest."},
                "mime": {"type": "string", "default": "text/markdown", "description": "MIME for content."},
                "doc_id": {"type": "string", "description": "Exact doc id (delete only)."},
                "force": {"type": "boolean", "default": false, "description": "Skip near-duplicate check on add."}
            },
            "required": []
        }),
    });

    // -----------------------------------------------------------------
    // A-share market data (astock). Tools no-op with a structured
    // "astock_not_configured" reply when the gateway hasn't been wired
    // to an astock instance — so they're always safe to advertise; the
    // LLM learns at first call that it can't use them and stops trying.
    // -----------------------------------------------------------------
    tools.push(ToolDef {
        name: "stock_quote".to_owned(),
        description: "Real-time A-share quote(s). Pass `code` (string) for one \
            quote, or `codes` (array) for a batch. Accepts codes in any common \
            form (600519, sh600519, SH:600519, 600519.SH — normalized internally). \
            Returns price, open/high/low, prior close, change %, volume, amount, \
            and timestamp. Cached ~5s on the gateway side so the LLM can ask the \
            same quote multiple times within a turn cheaply.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Single stock code."},
                "codes": {"type": "array", "items": {"type": "string"}, "description": "Multiple codes (batch)."},
                "format": {"type": "string", "enum": ["summary", "raw"], "default": "summary",
                           "description": "`summary` trims order-book levels (default); `raw` keeps bids/asks."}
            },
            "required": []
        }),
    });
    tools.push(ToolDef {
        name: "stock_kline".to_owned(),
        description: "Historical K-line bars for one A-share code. Use for \
            technical-analysis questions (\"近 60 日趋势\", \"创新高了吗\", \
            \"突破均线没有\"). Defaults: 200 daily bars (`period=1d`), no \
            adjustment. Set `adjust=qfq` for 前复权 / `hfq` for 后复权 when \
            comparing across dividend events.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Stock code (any common form)."},
                "period": {"type": "string", "enum": ["1m","5m","15m","30m","1h","1d","1w","1mon"],
                           "default": "1d", "description": "Bar granularity."},
                "count": {"type": "integer", "default": 200, "minimum": 1, "maximum": 800,
                          "description": "Number of bars to fetch."},
                "offset": {"type": "integer", "default": 0,
                           "description": "Skip N most-recent bars before counting."},
                "adjust": {"type": "string", "enum": ["none","qfq","hfq"], "default": "none"}
            },
            "required": ["code"]
        }),
    });
    tools.push(ToolDef {
        name: "stock_snapshot".to_owned(),
        description: "Full-market snapshot, sorted + capped. Use for broad market \
            questions where you want a ranked list of A-shares: \"今天涨幅前 20\", \
            \"沪市量比最高的票\", a watchlist refresh. The result is sorted by \
            `sort_by` (default `amount`) and trimmed to `limit` rows (default \
            50) so the LLM context stays bounded — the upstream service can \
            return 5000+ rows on a busy day. Filter by `market` (SH/SZ/BJ) or an \
            explicit `codes` list to narrow further. Set `ts=<date>` for a \
            historical snapshot.\n\
            \n\
            Smart default: if you pass NO `codes`, NO `market`, and NO `ts`, \
            the snapshot auto-filters to the caller's watchlist (see \
            `stock_watchlist`). The result envelope's `used_watchlist: true` \
            flag tells you whether that happened, so you can phrase the reply \
            as \"你关注的 N 只...\" instead of \"全市场...\". Override with \
            `use_watchlist=false` to force a market-wide snapshot.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "ts": {"type": "string", "description": "Historical timestamp (YYYY-MM-DD or RFC3339); omit for live."},
                "market": {"type": "string", "enum": ["SH","SZ","BJ"], "description": "Filter by exchange."},
                "codes": {"type": "array", "items": {"type": "string"}, "description": "Explicit code allowlist."},
                "adjust": {"type": "string", "enum": ["none","qfq","hfq"], "default": "none",
                           "description": "Only valid with `ts`; live snapshots are always raw."},
                "limit": {"type": "integer", "default": 50, "minimum": 1, "maximum": 5000,
                          "description": "Cap rows returned to the LLM."},
                "sort_by": {"type": "string", "enum": ["amount","pct","price"], "default": "amount",
                            "description": "Primary sort key."},
                "order": {"type": "string", "enum": ["desc","asc"], "default": "desc"},
                "use_watchlist": {"type": "boolean", "description": "When unset, defaults to the caller's watchlist if codes/market/ts are all empty. Pass false to force market-wide."}
            },
            "required": []
        }),
    });
    tools.push(ToolDef {
        name: "stock_ask".to_owned(),
        description: "PREFERRED A-share entry point — natural-language query \
            routed through iwencai (同花顺's question-understanding service via \
            astock). Use this whenever the user phrases a market question in \
            human terms: \"今天涨停的科技股有哪些\", \"北向资金净流入前 20\", \
            \"最近一周创业板连板高度榜\", \"机构调研最多的票\". Don't try to \
            assemble snapshot filters or SQL by hand for this kind of question — \
            iwencai handles the parsing and returns structured results that are \
            usually richer than what we'd compute locally.\n\
            \n\
            Falls back gracefully on empty / unknown queries: returns \
            `ok: false` with the upstream error so you can rephrase.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Free-form A-share question in Chinese or English."},
                "page": {"type": "integer", "description": "Result page (default 1)."},
                "limit": {"type": "integer", "description": "Page size (upstream default applies if omitted)."},
                "call_type": {"type": "string", "description": "Advanced iwencai hint; usually omit."}
            },
            "required": ["query"]
        }),
    });
    tools.push(ToolDef {
        name: "stock_chart".to_owned(),
        description: "Render a K-line + volume chart PNG for the user and send \
            it to the IM channel. DIFFERENT from `stock_kline` — that returns \
            raw bars for YOU to analyse; this one delivers a picture to the \
            USER's chat. Common pattern: call `stock_kline` first to read the \
            recent trajectory, draft your written analysis, then call \
            `stock_chart` so the user sees the picture beside your commentary.\n\
            \n\
            Defaults: 60 daily bars + MA5/10/20/60 overlays + volume subplot, \
            红涨绿跌 color convention (NOT US green/red). Pass `name` if you \
            know the Chinese stock name — it appears in the chart title.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Stock code (any common form)."},
                "name": {"type": "string", "description": "Chinese stock name to show in the title; optional."},
                "period": {"type": "string", "enum": ["1m","5m","15m","30m","1h","1d","1w","1mon"], "default": "1d"},
                "count": {"type": "integer", "default": 60, "minimum": 20, "maximum": 200},
                "adjust": {"type": "string", "enum": ["none","qfq","hfq"], "default": "none"},
                "ma": {"type": "array", "items": {"type": "integer"}, "default": [5,10,20,60],
                       "description": "MA overlay periods; pass `[]` to disable."}
            },
            "required": ["code"]
        }),
    });
    tools.push(ToolDef {
        name: "stock_watchlist".to_owned(),
        description: "Per-user persistent stock watchlist. Stored in rsclaw \
            memory, scoped to (channel, peer) so each IM user has their own \
            list across restarts. Use this when the user says \"加入关注\" / \
            \"关注一下 600519\" / \"以后每天给我看茅台\" — adding to the \
            watchlist lets downstream features (briefings, alerts, default \
            snapshot filter) personalize automatically.\n\
            \n\
            Actions:\n\
            - list — return all codes currently on the watchlist.\n\
            - add — codes:[...] adds N codes; existing entries are skipped.\n\
            - remove — codes:[...] removes by code.\n\
            - clear — wipe the entire watchlist for this peer.\n\
            \n\
            Don't proactively add stocks the user merely asked about once — \
            wait for explicit '关注/收藏/加入关注' intent.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list","add","remove","clear"], "default": "list"},
                "code": {"type": "string", "description": "Single stock code (alternative to `codes`)."},
                "codes": {"type": "array", "items": {"type": "string"}, "description": "Multiple stock codes."}
            },
            "required": []
        }),
    });
    tools.push(ToolDef {
        name: "research_analyze_charts".to_owned(),
        description: "Run a vision LLM over a batch of chart image URLs and \
            extract structured data (chart type, title, key numbers, trend). \
            Use this AS A FOLLOW-UP to `research_ingest_wechat` when the \
            ingested article is chart-heavy (TGB湖南人 复盘 series, 金融界 \
            morning reports, 卖方研究 stat sheets) — the text-only ingest \
            pulls section headers but not the numbers inside the charts. \
            Pass the `image_urls` array straight from the ingest tool's \
            response.\n\
            \n\
            Each image is fetched in parallel (with WeChat Referer to dodge \
            anti-hotlink), base64-encoded, sent in a single multimodal call. \
            Vision chain falls back per `agents.defaults.model.vision` \
            config. The LLM is instructed NOT to give investment advice — \
            just structured extraction.\n\
            \n\
            Returns `analysis` (markdown per chart) + `skipped` (per-image \
            failures, mostly 403/404). Save the analysis back into the KB \
            doc with `knowledge_base` add if you want it searchable.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "image_urls": {"type": "array", "items": {"type": "string"},
                               "description": "Chart image URLs (typically from `research_ingest_wechat`'s `image_urls` field)."},
                "max_images": {"type": "integer", "default": 8, "minimum": 1, "maximum": 10,
                               "description": "Cap how many images to send in one call. More charts = larger context = slower call."},
                "extra_prompt": {"type": "string", "description": "Optional extra instructions appended to the per-chart prompt (e.g. \"focus on 涨幅 vs 板块\")."}
            },
            "required": ["image_urls"]
        }),
    });
    tools.push(ToolDef {
        name: "research_ingest_wechat".to_owned(),
        description: "Fetch a WeChat 公众号 article by URL and ingest the title \
            / author / body / chart image URLs into the user's knowledge base. \
            Works on any `https://mp.weixin.qq.com/s/...` link the user pastes \
            or forwards — no browser, no OCR, no anti-bot dance. The body is \
            inlined in WeChat's response as a JS variable; we parse it server-\
            side. Charts come back as a separate URL list so a follow-up vision \
            LLM pass can extract data from them.\n\
            \n\
            Use when:\n\
            - User pastes / forwards a `mp.weixin.qq.com/s/...` URL\n\
            - User asks to \"save / collect this research / 收藏这篇\"\n\
            - You see a 研报 / 复盘 / 卖方报告 URL during a conversation\n\
            \n\
            The article lands in the knowledge base under `collection` \
            (default `research`), tagged `wechat_official_account`, account \
            name (e.g. `TGB湖南人`), and account id (e.g. `gh_xxx`). Later \
            `knowledge_base` searches can filter or rank by those tags.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "WeChat article URL (https://mp.weixin.qq.com/s/...)"},
                "collection": {"type": "string", "default": "research",
                               "description": "KB collection name. Created on demand."},
                "extra_tags": {"type": "array", "items": {"type": "string"},
                               "description": "Optional extra tags (e.g. 板块名, 个股代码) to attach for later filtering."}
            },
            "required": ["url"]
        }),
    });
    tools.push(ToolDef {
        name: "stock_query".to_owned(),
        description: "Read-only SQL against astock's DuckDB. ESCAPE HATCH only — \
            use when `stock_ask` / `stock_snapshot` / `stock_kline` genuinely \
            cannot express the query. astock validates the SQL server-side and \
            rejects anything that isn't a single SELECT. Joining the live K-line \
            tables and the EOD aggregates here is fine; we trust the validator.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "sql": {"type": "string", "description": "A single read-only SELECT."}
            },
            "required": ["sql"]
        }),
    });

    tools.push(ToolDef {
        name: "write_file".to_owned(),
        description: "Write/create a file (full-overwrite). Use this for ALL file creation and full \
            rewrites — do NOT use shell with notepad, echo, or any other editor/command to \
            create files.\n\
            \n\
            PREFER `edit_file` for modifying an existing file — it does an exact-string \
            replacement, sends a small diff, and is far cheaper than rewriting the whole file. \
            Use `write_file` only when (a) the file does not yet exist, or (b) you are \
            replacing the entire contents from scratch.\n\
            \n\
            If the file ALREADY exists, you must have read it (via `read_file`) in this \
            session before overwriting — otherwise you risk silently destroying content \
            that was added since you last looked.\n\
            \n\
            Do NOT create README.md, *.md documentation, or other narrative files unless \
            the user explicitly asked for documentation. Default to working code only.\n\
            \n\
            Creates parent directories as needed. Path is relative to workspace root.\n\
            Both 'path' and 'content' are required.\n\
            CRITICAL: When writing user-provided content, copy it EXACTLY character-by-character. \
            Never omit, rephrase, or regenerate numbers, dates, addresses, names, or any specific values. \
            If the user said '135号168栋', the content MUST contain '135号168栋' exactly.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "Relative file path, e.g. 'output.py'."},
                "content": {"type": "string", "description": "File content. MUST preserve all numbers/dates/values from the user verbatim."},
                "explanation": {"type": "string", "description": "Brief note on what you're creating and why."}
            },
            "required": ["path", "content"]
        }),
    });
    tools.push(ToolDef {
        name: "ask_user".to_owned(),
        description: "Pose a structured multi-choice question to the user.\n\
            \n\
            When to use:\n\
            - Multiple genuinely-viable approaches and the choice affects later work\n\
            - User's instruction omits a key parameter with no safe default\n\
            - Scope differs by an order of magnitude (small fix vs. big refactor)\n\
            - Aesthetic / preference decisions with no right answer\n\
            \n\
            Do NOT use for:\n\
            - 'Proceed?' / 'OK?' confirmations — just do the work the user asked for\n\
            - Fake-choice asks where one option is obviously right from context\n\
            - More than one ask_user in the same turn (collapse into one multi_select \
              or wait for the user's reply before the next ask)\n\
            \n\
            How it works (important — this is FIRE-AND-FORGET, not blocking):\n\
            - You call ask_user(question, options).\n\
            - The tool returns a `formatted_text` string. Send EXACTLY that text as your \
              reply to the user, then STOP this turn — do not continue working.\n\
            - The user's answer arrives as a normal message on your next turn. Parse it: \
              the number (\"1\", \"2\"), the option label, or free text (\"other\") are \
              all valid responses.\n\
            \n\
            If you recommend one option, set `recommended_index` (0-based). The formatter \
            marks it with '(Recommended)' so the user sees your steer.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The full question. Be specific — vague questions waste the user's time."
                },
                "options": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 8,
                    "items": {
                        "type": "object",
                        "properties": {
                            "label":       {"type": "string", "description": "Short option text (1-5 words)."},
                            "description": {"type": "string", "description": "Optional one-sentence explanation of what choosing this means."}
                        },
                        "required": ["label"]
                    },
                    "description": "2-8 distinct choices. Each entry is a {label, description} object, OR a bare string (the string becomes the label). Always include enough that 'Other' (free-text) isn't needed for the common path."
                },
                "multi_select": {
                    "type": "boolean",
                    "description": "If true, user may pick multiple options (comma-separated). Default: false."
                },
                "recommended_index": {
                    "type": "integer",
                    "description": "0-based index of the option YOU recommend. The formatter appends '(Recommended)'."
                },
                "header": {
                    "type": "string",
                    "description": "Optional short tag (e.g. 'Library', 'Approach') shown before the question."
                }
            },
            "required": ["question", "options"]
        }),
    });
    tools.push(ToolDef {
        name: "edit_file".to_owned(),
        description: "Modify an existing file by exact-string replacement.\n\
            \n\
            PREFER edit_file over write_file for any modification of an existing file — \
            it sends a small diff (old_string → new_string) instead of rewriting the whole \
            file, dramatically cheaper in tokens and far less likely to lose content.\n\
            \n\
            Usage:\n\
            - You must have called read_file on this path in the current session, so you \
              can copy a verbatim substring (including exact whitespace) as old_string.\n\
            - old_string must be UNIQUE in the file. If it appears more than once, either \
              extend old_string with surrounding lines until it's unique, or set \
              replace_all=true to replace every occurrence.\n\
            - old_string must be NON-EMPTY. To create a new file, use write_file.\n\
            - old_string and new_string must differ — identical strings are rejected.\n\
            \n\
            Common errors and how to fix them:\n\
            - 'not found': you misremembered the substring (whitespace, casing, or it was\n\
              edited earlier this turn). Re-read the file and copy verbatim.\n\
            - 'matches N locations': add 2-4 lines of surrounding context to old_string to\n\
              make it unique, OR pass replace_all=true if you really want every occurrence.\n\
            \n\
            Returns: { ok, path, replacements, bytes }. The 'replacements' field tells you\n\
            how many sites were rewritten (1 for normal mode, N for replace_all).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":        {"type": "string", "description": "Relative file path, e.g. 'src/main.rs'."},
                "old_string":  {"type": "string", "description": "Exact substring to replace (verbatim, incl. whitespace). Unique unless replace_all."},
                "new_string":  {"type": "string", "description": "Replacement; may be empty (= delete)."},
                "replace_all": {"type": "boolean", "default": false, "description": "Replace ALL occurrences (else requires uniqueness)."}
            },
            "required": ["path", "old_string", "new_string"]
        }),
    });
    tools.push(ToolDef {
        name: "send_file".to_owned(),
        description: "Send a file from the workspace to the user as an attachment. \
            Use this when the user asks you to send, share, or download a file. \
            The file will be delivered as a chat attachment (not as text). \
            Path is relative to workspace root.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to send (relative to workspace or absolute)"}
            },
            "required": ["path"]
        }),
    });
    tools.push(ToolDef {
        name: "shell".to_owned(),
        // OS-AGNOSTIC by design: this description is hashed into the
        // cacheable base-layer prefix, so it must be byte-identical across
        // macOS / Linux / Windows clients. The actual OS, shell, package
        // manager, and shell-specific syntax (chaining/quoting/encoding,
        // PowerShell edition quirks) live in the per-session `user_system`
        // "Platform" section (see prompt_builder::build_user_system), which
        // reflects the CLIENT's real OS and never enters the base hash.
        description: format!("Run a shell command. OS, shell, package manager, and chaining/\
             quoting syntax are in the system prompt's \"Platform\" section — follow those.\n\
             Prefer the dedicated tool when one exists: list_dir, search_file, \
             search_content, install_tool, web_fetch (HTTP), web_download (files). Shell is \
             for git, running scripts (node/python/cargo), system/process/package ops.\n\
             Pipe large output through head/tail; wait=false for long tasks; never run \
             destructive commands on personal dirs; on failure do NOT retry the same args — \
             change something or ask.\n\
             Multiple commands: independent → multiple shell calls in ONE message; \
             dependent → one call chained with the Platform operator. Never newline-separate.\n\
             Don't sleep: not between back-to-back commands, not to poll a wait=false task \
             (results are pushed to your next turn), not to retry a failure.\n\
             Git safety:\n\
             - NEVER force-push to main/master.\n\
             - NEVER `--no-verify` / `--no-gpg-sign` unless the user explicitly asks — fix the hook failure instead.\n\
             - AVOID `git add -A` / `git add .` — stage specific files (don't leak .env/credentials/artifacts).\n\
             - Prefer NEW commits over `--amend`; amending after a hook failure destroys work.\n\n{shell_guide}",
            shell_guide = crate::bootstrap::shell_guide()),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute. Must be valid for the current OS."},
                "timeout": {"type": "integer", "default": 30, "description": "Timeout seconds (max 300)."},
                "wait": {"type": "boolean", "default": true, "description": "Wait for finish (stdout/stderr/exit_code). false → long task, returns task_id to poll."},
                "task_id": {"type": "string", "description": "Poll a background task by its id."}
            },
            "required": []
        }),
    });
    tools.push(ToolDef {
        name: "agent".to_owned(),
        description: "Manage internal sub-agents. Actions: task (one-shot job, returns \
            task_id immediately, result delivered when done), spawn (persistent agent), \
            send (message an existing agent), list, update (model/name; model=\"\" resets), \
            kill. Use task for independent parallelizable work; pick a `toolset` matching \
            the job; after dispatching, tell the user and move on.\n\
            CRITICAL: when the user NAMES a CLI coding agent (claude/claudecode/openclaude/\
            opencode/codex), dispatch via `cap(agent=..., task=...)` — NOT this tool; \
            action=task creates an internal sub-agent and will never spawn the external CLI. \
            Claiming \"已委托 X\" without an actual `cap` call in the same turn is deception.\n\
            Do NOT delegate GUI/desktop automation, `computer_use` work, or visual \
            verification — sub-agents start without the live screen state and loop on \
            failure; do those yourself.\n\
            After dispatching a task, do NOT poll (list/send) or predict its findings — \
            completion is pushed to you. If you need the result to proceed, you delegated \
            wrong: do it inline.\n\
            Task prompt = the quality lever: brief it like a colleague with zero context — \
            goal, what you know, what NOT to redo, and an explicit exit criterion. For \
            lookups hand over the exact command; for investigations hand over the question. \
            One-line prompts produce shallow work.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["spawn", "task", "send", "list", "update", "kill"], "description": "Action to perform"},
                "id":      {"type": "string", "description": "Agent ID (for spawn/send/update/kill)"},
                "model":   {"type": "string", "description": "Model string (for spawn/task/update). Pass \"\" to remove per-agent model override."},
                "name":    {"type": "string", "description": "Display name (for update)"},
                "system":  {"type": "string", "description": "Role description (for spawn/task)"},
                "message": {"type": "string", "description": "Message to send (for task/send)"},
                "toolset": {"type": "string", "enum": ["minimal", "standard", "web", "code", "full"], "description": "Tool access level. Default: standard."}
            },
            "required": ["action"]
        }),
    });

    // Tool installer (structured alternative to exec rsclaw tools install).
    // enum/description derive from the available-tools list (cached manifest ∪
    // compiled-in baseline) so a new tool needs no edit here.
    {
        let cache = rsclaw_tools::load_manifest_cache_pub(&rsclaw_tools::tools_dir_pub());
        let available = rsclaw_tools::available_tools(cache.as_ref());
        tools.push(ToolDef {
            name: "install_tool".to_owned(),
            description: format!("Install a tool/runtime. Available: {}.", available.join(", ")),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "enum": available, "description": "Tool name to install"}
                },
                "required": ["name"]
            }),
        });
    }

    // File operation tools (structured alternatives to exec ls/find/grep).
    // These help small models avoid digit-loss and dead-loop issues.
    tools.push(ToolDef {
        name: "list_dir".to_owned(),
        description: "List files and directories in a given path.\n\
            Use this instead of shell with ls/dir.\n\
            - Returns file names, sizes, and types.\n\
            - Does not display hidden/dot files by default.\n\
            - Use 'pattern' to filter by glob (e.g. '*.json').\n\
            - Use 'recursive' to list subdirectories.\n\
            CRITICAL: 'path' must be returned before other parameters.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":      {"type": "string", "description": "Dir to list. Relative to workspace or absolute. E.g. '.', 'src/'."},
                "recursive": {"type": "boolean", "default": false, "description": "Recurse into subdirectories."},
                "pattern":   {"type": "string", "description": "Glob filter, e.g. '*.json'."}
            }
        }),
    });
    tools.push(ToolDef {
        name: "search_file".to_owned(),
        description: "Search for files by glob pattern. Use this instead of shell with find.\n\
            - Supports wildcard patterns for flexible matching.\n\
            - Returns absolute file paths sorted by MOST-RECENTLY-MODIFIED first, so\n\
              recently-touched files (the usually-relevant ones) bubble to the top.\n\
            - Prefer this over list_dir when you have a specific file pattern.\n\
            CRITICAL: 'pattern' must be returned before other parameters.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Filename glob, e.g. 'test_*.py', '**/*.rs'."},
                "path":    {"type": "string", "description": "Root dir. Default workspace root."},
                "max_results": {"type": "integer", "default": 20, "description": "Max results."}
            },
            "required": ["pattern"]
        }),
    });
    tools.push(ToolDef {
        name: "search_content".to_owned(),
        description: "Search file contents by regex or literal pattern.\n\
            Prefers ripgrep (`rg`) when installed — supports `output_mode` and `multiline`,\n\
            respects .gitignore, fast. Falls back to `grep -rn` (Unix) / `Select-String`\n\
            (Windows) when `rg` is absent; on the fallback path `output_mode` and\n\
            `multiline` are silently ignored.\n\
            - Use this instead of shell with grep/rg.\n\
            - Full regex syntax: 'log.*Error', 'function\\s+\\w+', 'TODO|FIXME'.\n\
            - Escape literal regex metacharacters: 'functionCall\\(', 'interface\\{\\}'.\n\
            - `output_mode`: 'content' (default; lines with matches), 'files_with_matches'\n\
              (file paths only — much cheaper for surveys), 'count' (per-file match count).\n\
            - `multiline: true` enables patterns that span lines (e.g. 'struct\\s+\\w+\\{[\\s\\S]*?field').\n\
            - Use 'include' glob to filter by file type: '*.py', '*.rs', '*.{ts,tsx}'.\n\
            CRITICAL: 'pattern' must be returned before other parameters.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern":  {"type": "string", "description": "Regex to search for. E.g. 'class\\s+\\w+', 'def main'."},
                "path":     {"type": "string", "description": "File/dir to search. Default workspace root."},
                "include":  {"type": "string", "description": "Glob filter, e.g. '*.{ts,tsx}'."},
                "ignore_case": {"type": "boolean", "default": false, "description": "Case-insensitive match."},
                "max_results": {"type": "integer", "default": 20, "description": "Max results."},
                "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "default": "content", "description": "files_with_matches is cheaper when you only need WHICH files match. (ripgrep only)"},
                "multiline":   {"type": "boolean", "default": false, "description": "Cross-line matching. (ripgrep only)"}
            },
            "required": ["pattern"]
        }),
    });

    // Web tools.
    tools.push(ToolDef {
        name: "web_search".to_owned(),
        description: format!("Search the web for real-time information.\n\
            When to use:\n\
            - Questions beyond your knowledge cutoff or training data\n\
            - Current events, recent updates, time-sensitive information\n\
            - Latest documentation, API references, version-specific features\n\
            - When unsure about facts — search BEFORE saying 'I don't know'\n\
            Tips:\n\
            - Be specific: include version numbers, dates, or exact terms\n\
            - Include the CURRENT year in queries when looking for latest docs / news\n\
              (the model's training cutoff is older than \"now\"; queries written for\n\
              last year's docs return last year's results).\n\
            - For Chinese content, search in Chinese for better results\n\
            \n\
            After answering with web_search results, list your sources at the end as:\n\
              Sources:\n\
              - [Title](URL)\n\
              - [Title](URL)\n\
            so the user can verify the claim. Unsourced web-search-backed claims feel\n\
            invented and erode trust.\n\n{}", crate::bootstrap::web_search_guide()),
        parameters: json!({
            "type": "object",
            "properties": {
                "query":    {"type": "string", "description": "Search query — specific keywords + dates."},
                "provider": {"type": "string", "description": "duckduckgo | google | bing | brave. Empty = default."},
                "limit":    {"type": "integer", "default": 5, "description": "Max results."},
                "deep":     {"type": "boolean", "default": false, "description": "When true, don't return snippets — fetch the top pages' full text, rerank the most relevant passages against the query, and return those text chunks with their source URLs. Use for precision questions where snippets are too thin (analysis, comparisons, 'what does X actually say'). Slower (a few seconds); skip for quick lookups."}
            },
            "required": ["query"]
        }),
    });
    tools.push(ToolDef {
        name: "web_fetch".to_owned(),
        description: format!("PREFERRED tool for HTTP — web pages, REST APIs, docs. Never \
            shell out to curl/wget/Invoke-WebRequest for plain requests.\n\
            Full URL required; HTML is dehydrated to clean text; JSON/plain-text returned \
            as-is; JS-heavy or CAPTCHA-blocked pages auto-fall-back to browser rendering \
            (GET only); plain GETs cached ~15 min.\n\
            `prompt`: pass ONLY for large HTML pages to LLM-extract specific info. For \
            compact structured responses (JSON APIs / RSS / <2KB text) OMIT it — it adds \
            a 30-60s summarize pass for nothing.\n\
            method GET/POST/PUT/PATCH/DELETE; headers object for auth; body string raw or \
            object auto-JSON-serialized.\n\
            Fall back to shell+curl only for multipart upload or incremental SSE/chunked \
            streaming; interactive-login sites need web_browser.\n\
            GitHub: prefer `gh` CLI via shell (gh pr view / gh issue view / gh api) — it \
            returns structured data where fetching github.com HTML gives a dehydrated UI.\n\
            If the response shows a redirect to a DIFFERENT host, re-fetch the redirect \
            URL — don't assume the original content was served.\n\n{}", crate::bootstrap::web_fetch_guide()),
        parameters: json!({
            "type": "object",
            "properties": {
                "url":     {"type": "string", "description": "Full URL to fetch (e.g. https://docs.example.com/api)"},
                "method":  {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"], "description": "HTTP method. Default: GET"},
                "headers": {"type": "object", "description": "Optional request headers, e.g. {\"Authorization\": \"Bearer xyz\", \"X-API-Key\": \"...\"}", "additionalProperties": {"type": "string"}},
                "body":    {"description": "Optional request body. String → sent as-is (set Content-Type via headers). Object/array → JSON-serialized; Content-Type defaulted to application/json."},
                "prompt":  {"type": "string", "description": "OPTIONAL. What to extract from a LARGE HTML page (e.g. 'list all API endpoints'). Triggers an LLM-summarize pass on the response. OMIT for compact JSON/XML/text where you can read the raw body — passing 'prompt' on small structured responses just adds ~30-60 s of LLM latency for nothing."}
            },
            "required": ["url"]
        }),
    });
    tools.push(ToolDef {
        name: "web_download".to_owned(),
        description: "Download a file (image/video/document/archive) from URL to local path.\n\
            - Supports resume for large files\n\
            - Use use_browser_cookies=true for authenticated downloads (e.g. after logging in via web_browser)\n\
            - Path is relative to workspace/downloads/ — just use filename like 'photo.jpg'\n\
            - Do NOT use shell with curl/wget — always use this tool\n\
            - After downloading, use send_file to deliver the file to the user".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "url":  {"type": "string", "description": "Full URL to download"},
                "path": {"type": "string", "description": "Destination filename (e.g. 'video.mp4', 'report.pdf'). Relative to workspace/downloads/."},
                "cookies": {"type": "string", "description": "Cookie header string, e.g. 'session=abc; token=xyz'"},
                "use_browser_cookies": {"type": "boolean", "description": "Auto-extract cookies from active browser session for this URL's domain (use after web_browser login)"}
            },
            "required": ["url", "path"]
        }),
    });
    tools.push(ToolDef {
        name: "web_browser".to_owned(),
        description: format!("Control a web browser. Workflow: `open` URL → `snapshot` \
            (returns interactive element refs @e1, @e2…; `interactive: true` = actionable \
            elements only, saves tokens; add `annotate: true` for a labeled screenshot \
            alongside) → `click` ref=@e1 / `fill` ref=@e2 text='…' → re-snapshot after any \
            page change (refs go stale).\n\
            Autocomplete inputs (city pickers, search boxes, anything that pops a suggestion \
            dropdown): ALWAYS one `pick` ref=@eN query='武汉' call — it types, waits for the \
            popup, clicks the first candidate, and survives IME/React inputs. NEVER hand-roll \
            click+type+wait+screenshot loops for these; the dropdown dismisses before you \
            re-screenshot.\n\
            More actions: hover/dblclick/drag(from,to)/focus/scrollintoview; `search` \
            (auto-find any site's search box, fill, submit); `clickAt` (real CDP mouse click \
            — file dialogs, anti-bot); getbytext/getbyrole/getbylabel (locate without @ref); \
            frame/mainframe (iframes); console; content (full HTML); waitforurl; plus type, \
            select, check, scroll, screenshot, pdf, press, back, forward, reload, wait, \
            evaluate, cookies, get_text, get_url, get_title, find, get_article, upload, \
            new_tab, switch_tab, close_tab.\n\
            Site-rules: verified per-site selectors/routes/gotchas live under \
            `~/.rsclaw/tools/web_browser/site-rules/` as `<domain>.md` or \
            `<domain>/<task>.md`. When opening a matching host, read_file the rule FIRST \
            instead of guessing selectors. Before using a nested `<domain>/<task>.md`, read \
            `site-rules/_VOCABULARY.md` once per session — it maps imported helper syntax \
            (click_at_xy/type_text/js) to your actions.\n\
            Screenshot routing: web page → `action=screenshot url=…` (navigates then \
            captures, one call). Desktop screenshot → NOT this tool; tell the user to type \
            `/ss`. Bare `action=screenshot` with no url captures the persistent session — \
            usually a blank tab; don't.\n\n{}", crate::bootstrap::web_browser_guide()),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":     {"type": "string", "enum": [
                    "open", "navigate", "snapshot", "click", "clickAt", "fill", "type", "pick",
                    "select", "check", "uncheck", "scroll", "screenshot", "pdf",
                    "hover", "dblclick", "drag", "focus", "scrollintoview",
                    "back", "forward", "reload", "get_text", "get_url", "get_title",
                    "wait", "evaluate", "cookies", "press", "set_viewport",
                    "dialog", "state", "network", "new_tab", "list_tabs",
                    "switch_tab", "close_tab", "highlight", "clipboard", "find",
                    "get_article", "upload", "context", "emulate", "diff", "record",
                    "search", "console", "content", "frame", "mainframe",
                    "waitforurl", "getbytext", "getbyrole", "getbylabel"
                ]},
                "url":        {"type": "string", "description": "URL for open/navigate"},
                "interactive":{"type": "boolean", "description": "For snapshot: only return actionable elements (saves ~80% tokens). Default: false"},
                "annotate":   {"type": "boolean", "description": "For snapshot (with interactive=true): overlay numbered labels on elements and return a screenshot + legend alongside the structured text."},
                "ref":        {"type": "string", "description": "Element ref like @e3 from snapshot"},
                "from":       {"type": "string", "description": "Source element ref for drag"},
                "to":         {"type": "string", "description": "Target element ref for drag"},
                "x":          {"type": "number", "description": "X pixel coordinate for clickAt"},
                "y":          {"type": "number", "description": "Y pixel coordinate for clickAt"},
                "text":       {"type": "string", "description": "Text for fill/type/click-by-text/clipboard/dialog"},
                "query":      {"type": "string", "description": "Pick: text to type and match against dropdown candidates"},
                "index":      {"type": "integer", "description": "Pick: candidate index, 0-based (default 0)"},
                "timeout_ms": {"type": "integer", "description": "Pick: dropdown wait in ms (default 5000)"},
                "value":      {"type": "string", "description": "Select value, or sub-action for cookies/state/dialog/network/clipboard/context/emulate/diff/record"},
                "key":        {"type": "string", "description": "Key name for press (Enter, Tab, Escape, etc.)"},
                "direction":  {"type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction"},
                "amount":     {"type": "integer", "description": "Scroll distance in pixels (default 500)"},
                "selector":   {"type": "string", "description": "CSS selector for scroll container"},
                "js":         {"type": "string", "description": "JavaScript for evaluate action"},
                "target":     {"type": "string", "description": "Wait target: CSS selector, text, url, networkidle, fn"},
                "timeout":    {"type": "number", "description": "Timeout in seconds (default 15)"},
                "format":     {"type": "string", "enum": ["png", "jpeg"], "description": "Screenshot format"},
                "quality":    {"type": "integer", "description": "JPEG quality (1-100)"},
                "full_page":  {"type": "boolean", "description": "Capture full scrollable page"},
                "width":      {"type": "integer", "description": "Viewport width for set_viewport"},
                "height":     {"type": "integer", "description": "Viewport height for set_viewport"},
                "scale":      {"type": "number", "description": "Device scale factor for set_viewport"},
                "mobile":     {"type": "boolean", "description": "Mobile emulation for set_viewport"},
                "target_id":  {"type": "string", "description": "Tab target ID for switch_tab/close_tab"},
                "state":      {"type": "object", "description": "State object for state load"},
                "pattern":    {"type": "string", "description": "URL pattern for network block/intercept"},
                "by":         {"type": "string", "enum": ["text", "label"], "description": "Find element by text or label"},
                "then":       {"type": "string", "description": "Action after find (click)"},
                "cookie":     {"type": "object", "description": "Cookie object for cookies set"},
                "files":      {"type": "array", "items": {"type": "string"}, "description": "File paths for upload"},
                "context_id": {"type": "string", "description": "Browser context ID for cookie isolation"},
                "latitude":   {"type": "number", "description": "Latitude for geolocation emulation"},
                "longitude":  {"type": "number", "description": "Longitude for geolocation emulation"},
                "accuracy":   {"type": "number", "description": "Geolocation accuracy in meters"},
                "locale":     {"type": "string", "description": "Locale for emulation (e.g. en-US, zh-CN)"},
                "timezone_id":{"type": "string", "description": "IANA timezone (e.g. Asia/Shanghai)"},
                "permissions":{"type": "array", "items": {"type": "string"}, "description": "Browser permissions to grant"},
                "action_type":{"type": "string", "description": "Intercept action: block or mock"},
                "body":       {"type": "string", "description": "Mock response body for network intercept"},
                "headed":     {"type": "boolean", "description": "true=visible window, false=headless. Omit to auto-detect."}
            },
            "required": ["action"]
        }),
    });

    tools.push(ToolDef {
        name: "computer_use".to_owned(),
        description: "Control the computer desktop. ONLY use when the user EXPLICITLY asks to take a screenshot, click, type, or interact with the desktop. Do NOT call this tool just because the message mentions words like 'screenshot' or 'screen' in other contexts. Screenshots auto-resize, and mouse coordinates use the same physical-pixel space as the returned `original_width`/`original_height` (HiDPI is handled internally — multiply image-pixel coords by the returned `scale` and pass directly).\n\nCRITICAL — App-Rule Loading (MANDATORY):\nBefore controlling ANY desktop application (WeChat, Finder, Safari, etc.), you MUST first load the app-specific automation guide.\n1. Call `get_app_rule` with the app name (e.g. name='wechat').\n2. Read and follow the returned guide EXACTLY.\n3. Only then proceed with screenshot/click/type actions.\nNEVER guess coordinates or workflows — the app-rule contains version-specific steps you cannot infer.\n\nEND-TO-END AUTOMATION — vlm_drive action:\nFor complex multi-step desktop tasks (e.g. 'Open WeChat, find a group, send a message'), use the `vlm_drive` action with a natural-language instruction instead of scripting individual click/type steps. The configured VLM will drive the loop internally: screenshot -> analyze -> execute -> repeat. This is more robust than hand-coding coordinates.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":    {"type": "string", "enum": [
                    "screenshot", "mouse_move", "mouse_click", "left_click",
                    "double_click", "triple_click", "right_click", "middle_click",
                    "drag", "scroll", "type", "key", "hold_key",
                    "cursor_position", "get_active_window", "ui_tree",
                    "list_app_rules", "get_app_rule", "wait",
                    "vlm_drive"
                ], "description": "ui_tree = accessibility tree of focused window. list_app_rules/get_app_rule = per-app automation playbooks. vlm_drive = end-to-end VLM loop driven by `instruction`."},
                "x":         {"type": "number", "description": "X coordinate (mouse actions, drag start) in physical pixels"},
                "y":         {"type": "number", "description": "Y coordinate (mouse actions, drag start) in physical pixels"},
                "to_x":      {"type": "number", "description": "Drag destination X (physical pixels)"},
                "to_y":      {"type": "number", "description": "Drag destination Y (physical pixels)"},
                "button":    {"type": "string", "enum": ["left", "right", "middle"], "description": "Mouse button (default: left)"},
                "text":      {"type": "string", "description": "Text for type action"},
                "key":       {"type": "string", "description": "Key name or combo (e.g. Enter, ctrl+c, cmd+shift+s)"},
                "then":      {"type": "string", "enum": ["click", "double_click", "right_click", "triple_click"], "description": "Sub-action for hold_key (default: click)"},
                "direction": {"type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction (default: down)"},
                "amount":    {"type": "integer", "description": "Scroll clicks (default: 3)"},
                "ms":        {"type": "integer", "description": "Wait duration in milliseconds (max 10000)"},
                "name":      {"type": "string", "description": "App-rule name (for get_app_rule action)"},
                "region":    {"type": "object", "description": "Screenshot sub-region in physical pixels (zoom-in after a full shot).", "properties": {
                    "x":      {"type": "number"},
                    "y":      {"type": "number"},
                    "width":  {"type": "number"},
                    "height": {"type": "number"}
                }, "required": ["x", "y", "width", "height"]},
                "max_long_edge_px": {"type": "integer", "description": "Screenshot longest-edge cap, 64-8192 (default 1024). Larger = more detail + tokens."},
                "instruction": {"type": "string", "description": "vlm_drive only: natural-language task (e.g. 'Open WeChat, find group X, send a greeting')."},
                "max_steps": {"type": "integer", "description": "Maximum steps for the vlm_drive loop (default: 30). ONLY used with action=vlm_drive."},
                "async": {"type": "boolean", "description": "vlm_drive only: true = background, returns task_id immediately (use for anything >30s; acknowledge the user, outcome arrives later). false (default) blocks up to 4 min — quick visual queries only."}
            },
            "required": ["action"]
        }),
    });

    // --- New openclaw-compatible tools ---

    tools.push(ToolDef {
        name: "image_gen".to_owned(),
        description: "Generate or EDIT an image with an AI image model. Text-to-image (prompt only) or image-to-image (pass `image` to transform/edit an existing picture). Pass the user's original description as-is (preserve their language, do not translate).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Image description, or for image-to-image the edit instruction (what to change/keep). Use the user's original language and wording, do not translate."},
                "size":   {"type": "string", "description": "Image size, e.g. 2048x2048", "default": "2048x2048"},
                "image":  {"type": ["string", "array"], "items": {"type": "string"}, "description": "Optional input image(s) for image-to-image / editing (agnes, doubao seedream, and OAI-compatible gateways). Each entry: a LOCAL FILE PATH (e.g. a prior image_gen result — auto-encoded to base64), a public https URL, or a data:image/...;base64,... URI. One image = edit/transform it; multiple = multi-image composition (agnes)."}
            },
            "required": ["prompt"]
        }),
    });
    tools.push(ToolDef {
        name: "ocr".to_owned(),
        description: "Extract text from an image via OCR (rsclaw-ocr-v1). Use when the user \
            sends a screenshot/photo/scan and wants the TEXT read out verbatim (receipts, \
            documents, slides, chat screenshots) — distinct from describing what an image \
            shows. Pass a file reference path, an http(s) URL, or a data URI. Requires an \
            `kb.ocr` endpoint configured; if absent the call returns an error.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "image": {"type": "string", "description": "Image to OCR: a workspace/uploads file path, an http(s):// URL, or a data:image/...;base64,... URI."},
                "lang":  {"type": "string", "description": "Optional language hint (zh/en/...). Omit to auto-detect."}
            },
            "required": ["image"]
        }),
    });
    tools.push(ToolDef {
        name: "video_gen".to_owned(),
        description: "Generate a video from a text description using an AI video model. \
            Use this tool whenever the user asks to: create a video, animate an image, \
            generate a clip, make a short film, produce footage, or anything involving \
            video output. Pass the user's original description as-is (preserve their \
            language, do not translate).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt":       {"type": "string", "description": "Video description. Use the user's original language and wording."},
                "duration":     {"type": "integer", "description": "Duration in seconds (default: 5)", "default": 5},
                "aspect_ratio": {"type": "string", "description": "Aspect ratio: 16:9, 9:16, 1:1 (default: 16:9)", "default": "16:9"},
                "image":        {"type": ["string", "array"], "items": {"type": "string"}, "description": "Optional reference image(s) for image-to-video (agnes, doubao, rsclaw). Each entry: a LOCAL FILE PATH (e.g. the path returned by image_gen — auto-encoded to base64), a public https URL, or a data:image/...;base64,... URI. ONE image = first frame (animate this image). TWO images = first + last frame (interpolate). THREE+ = reference images (doubao multimodal)."},
                "model":        {"type": "string", "description": "Video model to use, e.g. doubao seedance, agnes, rsclaw, sora-2 (optional, uses configured default)"}
            },
            "required": ["prompt"]
        }),
    });
    tools.push(ToolDef {
        name: "avatar_gen".to_owned(),
        description: "Generate a talking-avatar / 数字人 video via the rsclaw gen service: a face \
            portrait + a speech audio clip → a lip-synced video. Use when the user wants a digital \
            human / virtual presenter / 数字人口播 from a photo and a voice clip. There is NO text \
            prompt — supply the speech as AUDIO. The finished video is delivered automatically.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "image": {"type": "string", "description": "REQUIRED: face portrait — a LOCAL FILE PATH (auto-encoded to base64), a public https URL, or a data:image/...;base64,... URI."},
                "audio": {"type": "string", "description": "REQUIRED: speech audio to lip-sync — a LOCAL FILE PATH (wav/mp3/flac/m4a, auto-encoded to base64), a public https URL, or a data:audio/...;base64,... URI."},
                "model": {"type": "string", "description": "Optional rsclaw gen avatar model id (uses configured default when empty)."}
            },
            "required": ["image", "audio"]
        }),
    });
    tools.push(ToolDef {
        name: "mv_gen".to_owned(),
        description: "Generate a music-video (MV) via the rsclaw gen service (lyrics + reference \
            image → music video). Use when the user wants an MV / music-driven clip. The finished \
            video is delivered automatically. Pass the user's original wording as-is.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Style / theme description for the MV. Use the user's original language."},
                "lyrics": {"type": "string", "description": "Optional song lyrics to drive the MV / vocals."},
                "image":  {"type": "string", "description": "Optional reference image: a LOCAL FILE PATH (auto-encoded to base64), a public https URL, or a data:image/...;base64,... URI."},
                "model":  {"type": "string", "description": "Optional rsclaw gen mv model id (uses configured default when empty)."}
            },
            "required": ["prompt"]
        }),
    });
    tools.push(ToolDef {
        name: "audio_gen".to_owned(),
        description: "Generate AUDIO via the rsclaw gen service. Two modes via `kind`: \
            `music` (style/lyrics → a song) and `voice` (text → speech, with optional one-shot \
            voice cloning from a reference clip). Returns the finished audio as an attachment. \
            Use for: 生成歌曲/音乐/BGM (kind=music), or 文字转语音/配音/语音克隆 (kind=voice). \
            Pass the user's original wording as-is.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "kind":   {"type": "string", "enum": ["music", "voice"], "description": "music = song from a style/lyrics; voice = text-to-speech / voice clone.", "default": "music"},
                "prompt": {"type": "string", "description": "music ONLY: style description (e.g. 'upbeat lo-fi hip hop'). Required for kind=music."},
                "lyrics": {"type": "string", "description": "music ONLY: optional song lyrics (with vocals)."},
                "duration": {"type": "integer", "description": "music ONLY: target length in seconds."},
                "text":   {"type": "string", "description": "voice ONLY: the words to speak. Required for kind=voice."},
                "voice":  {"type": "string", "description": "voice ONLY: a preset voice name or a natural-language voice description (e.g. '温柔女声')."},
                "instructions": {"type": "string", "description": "voice ONLY: optional delivery/style direction (e.g. '低沉磁性, 语速偏慢')."},
                "speed":  {"type": "number", "description": "voice ONLY: speaking speed 0.25-4.0 (default 1.0)."},
                "reference_audio": {"type": "string", "description": "voice ONLY: optional clone reference — a LOCAL FILE PATH (auto-encoded), an https URL, or a data:audio/...;base64,... URI."},
                "reference_text":  {"type": "string", "description": "voice ONLY: transcript of reference_audio, improves clone fidelity."},
                "response_format": {"type": "string", "description": "Output container: mp3 (default) | wav | flac.", "default": "mp3"},
                "model":  {"type": "string", "description": "Optional rsclaw gen model id (uses configured default per kind when empty)."}
            },
            "required": []
        }),
    });
    tools.push(ToolDef {
        name: "pdf".to_owned(),
        description: "Extract text content from a PDF file or URL.\n\
            - Supports local files and remote URLs.\n\
            - Returns extracted text suitable for analysis.\n\
            - For large PDFs, content may be truncated.\n\
            Example: {\"path\":\"report.pdf\"} or {\"path\":\"https://example.com/doc.pdf\"}".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "REQUIRED: File path (relative to workspace) or full URL. Examples: 'docs/report.pdf', 'https://example.com/whitepaper.pdf'"}
            },
            "required": ["path"]
        }),
    });
    tools.push(ToolDef {
        name: "text_to_voice".to_owned(),
        description: "Convert text to speech audio and send as voice message.\n\
            - Generates audio from text input.\n\
            - On macOS uses 'say', on Linux uses espeak/sherpa-onnx.\n\
            - Result is sent as a voice attachment to the user.\n\
            Example: {\"text\":\"Hello world\",\"voice\":\"Tingting\"}".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "text":  {"type": "string", "description": "REQUIRED: Text to convert to speech. Can be any language."},
                "voice": {"type": "string", "description": "Voice name. macOS: run 'say -v ?' for list. Linux: run 'espeak --voices'. Examples: 'Tingting' (Chinese), 'Samantha' (English)"}
            },
            "required": ["text"]
        }),
    });
    tools.push(ToolDef {
        name: "send_message".to_owned(),
        description: "Send a message to a chat channel target (user or group).\n\
            Use this to proactively reach out to users on messaging platforms.\n\
            Channel is auto-detected from current session if not specified.\n\
            Example: {\"target\":\"user123\",\"text\":\"Task completed!\",\"channel\":\"telegram\"}".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string", "description": "Channel type. Examples: 'telegram', 'discord', 'feishu', 'weixin', 'slack'"},
                "target":  {"type": "string", "description": "REQUIRED: Target user ID or group/chat ID"},
                "text":    {"type": "string", "description": "REQUIRED: Message text to send"}
            },
            "required": ["target", "text"]
        }),
    });
    tools.push(ToolDef {
        name: "cron".to_owned(),
        // Deliberately terse: the decision rules below are HEADS only.
        // Enforcement moved to the add-path — validate_cron_expr rejects
        // bad expressions with a teaching message, and the add result
        // echoes back what was scheduled (one-shot vs forever, verbatim
        // vs agent run) so the model can self-correct in the same turn.
        // History: this desc was a 1.3k-token decision-tree manual; the
        // add-result echo made the prevention prose redundant.
        description: "Schedule jobs. Actions: list, add, edit, remove, enable, disable \
            (address jobs by `index` from list output).\n\
            Schedule — pick by user intent:\n\
            - ONE-SHOT `delay_ms`: a specific time that fires ONCE (\"晚上8点提醒我\", \
            \"15分钟后\", \"明天下午3点\"). Compute ms from now. Auto-removes after firing. \
            NEVER express one-time intent as a recurring expr — it fires every day forever.\n\
            - RECURRING `schedule` (5-field cron expr: minute hour day month weekday, \
            e.g. \"30 8 * * 1-5\") or `every_seconds` — ONLY when the user says \
            每天/每周/每隔/every/hourly/weekly.\n\
            `kind`: agentTurn (default) runs the agent with tools at fire time — required \
            whenever `message` is something to DO (query/check/fetch). systemEvent delivers \
            `message` text verbatim, no LLM — only for fixed reminder text.\n\
            `iter` (rotate): user wants ONE item per fire, cycling (轮流/依次/one at a time) \
            — pass the items array and use {current} in `message`. A list WITHOUT a rotation \
            signal = one job naming all items, not iter.\n\
            Cancelling: \"停掉/取消/cancel X\" → list, then remove. The job keeps firing \
            until actually removed — never just acknowledge.\n\
            /loop self-termination: when fired by a `cron:loop-<12hex>` session and the \
            loop's goal is met, call cron(action=\"remove\", id=\"loop-<12hex>\").".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":        {"type": "string", "enum": ["list", "add", "edit", "remove", "enable", "disable"], "description": "Action to perform"},
                "schedule":      {"type": "string", "description": "Cron schedule expression (for add/edit recurring jobs). Must be 5 whitespace-separated fields."},
                "every_seconds": {"type": "number", "description": "Fire every N seconds (for add). Use for fixed intervals like 45 minutes (every_seconds=2700) that cannot be expressed as a 5-field cron expression."},
                "delay_ms":      {"type": "number", "description": "Delay in milliseconds for one-shot timer (e.g., 1200000 = 20 min). Use instead of schedule for reminders/timers."},
                "message":       {"type": "string", "description": "Message or task to run (for add, edit)"},
                "kind":          {"type": "string", "enum": ["agentTurn", "systemEvent"], "description": "agentTurn (default) = run agent at fire time. systemEvent = deliver `message` verbatim, no agent run."},
                "index":         {"type": "number", "description": "Job index from list (1-based, for edit/remove/enable/disable - preferred)"},
                "id":            {"type": "string", "description": "Job ID (for edit/remove/enable/disable - use index instead if possible)"},
                "name":          {"type": "string", "description": "Job name (for add, edit)"},
                "tz":            {"type": "string", "description": "Timezone IANA name. Auto-detected if omitted. Only set if user explicitly requests a different timezone."},
                "agentId":       {"type": "string", "description": "Agent ID to run the job (for add, edit, default: main)"},
                "iter":          {"type": "array", "items": {"type": "string"}, "description": "Round-robin items, one per firing; placeholders {current}/{next}/{index}/{total} in `message`. On edit: new array replaces items, [] clears."},
                "iter_cursor":   {"type": "number", "description": "On `edit`: explicitly set the iter cursor (0-based). Use to reset rotation back to the start, or to jump to a specific item. Without `iter`, requires the job to already have iter configured."}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "session".to_owned(),
        description: "Manage sessions. Actions: send (message to another agent), list (all active sessions), history (retrieve conversation), status (session info).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":     {"type": "string", "enum": ["send", "list", "history", "status"], "description": "Action to perform"},
                "agentId":    {"type": "string", "description": "Target agent ID (for send)"},
                "sessionKey": {"type": "string", "description": "Session key (for send/history/status)"},
                "message":    {"type": "string", "description": "Message text (for send)"},
                "limit":      {"type": "number", "description": "Max messages to return (for history, default 50)"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "gateway".to_owned(),
        description: "Query gateway status and information.\n\
            - status: Current gateway state, uptime, connected channels, active agents\n\
            - health: Health check (OK/degraded)\n\
            - version: Gateway version and build info".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["status", "health", "version"], "description": "REQUIRED: Info to retrieve. Examples: 'status', 'version'"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "cap_live".to_owned(),
        description: "Interactive (synchronous) call into a CLI coding agent that keeps its \
            session warm across multiple turns. Use this — NOT `cap` — when you want to chain \
            several requests against the same agent (e.g. ask codex to design, then ask the \
            same codex session to refine), or to orchestrate multiple agents in one IM turn \
            (codex designs → claudecode implements → opencode reviews, each with their own \
            session_id). Returns the agent's full reply synchronously, plus a `session_id` you \
            pass back on subsequent calls to continue the same warm session. Omit `session_id` \
            (or pass an empty string) to open a brand-new session. Call `cap_live_end` when done. \
            For one-shot fire-and-forget work where you don't need to read the agent's reply in \
            this turn, prefer the async `cap` tool instead — it returns immediately and pushes \
            the completion summary to the user via IM."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": ["claudecode", "openclaude", "opencode", "codex", "qoder"],
                    "description": "Which CLI coding agent to dispatch to. A session is locked \
                        to one agent — you can't pass session_id from a codex call to a \
                        claudecode call."
                },
                "task":       { "type": "string", "description": "Prompt for the agent (this turn)." },
                "session_id": { "type": "string", "description": "Optional. Pass back the value \
                    returned from a prior cap_live call to continue the same session. Omit or \
                    leave empty to open a new session." },
                "cwd":        { "type": "string", "description": "Optional working directory for \
                    NEW sessions (ignored when continuing an existing session_id)." }
            },
            "required": ["agent", "task"]
        }),
    });
    tools.push(ToolDef {
        name: "cap_live_end".to_owned(),
        description: "Release a `cap_live` session and tear down its underlying driver process. \
            Always call this when you're done orchestrating a multi-turn cap session, both to \
            free the global session limit (default 8) and to avoid keeping coding-CLI processes \
            warm in the background. Idle sessions auto-close after 10 minutes anyway, but \
            explicit cleanup is good citizenship."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "session_id returned by a prior cap_live call." }
            },
            "required": ["session_id"]
        }),
    });
    tools.push(ToolDef {
        name: "cap_bind_sticky".to_owned(),
        description: "Hand the conversation off to a CLI coding agent in STICKY DIRECT MODE — \
            from the next user message onward, the user's messages flow STRAIGHT to the cap \
            driver, completely BYPASSING this main LLM. Call this when the user explicitly \
            asks to switch to a coding agent for the next several turns: \
            \"接下来让 claudecode 来\", \"switch to codex\", \"用 opencode 帮我做\", \
            \"let claude take over\". Equivalent to the `/cap <agent>` slash command but \
            invoked via tool. Use `cap_unbind_sticky` when the user wants to come back. \n\
            \n\
            Tradeoff vs `cap_live`: cap_live keeps YOU in the loop (you read each cap reply \
            and decide what to send next, can orchestrate multiple agents). cap_bind_sticky \
            REMOVES YOU from the loop — the user talks to the coding agent directly. Pick \
            cap_bind_sticky when the user signals \"step out of the way\"; pick cap_live \
            when the user asks for orchestration / wants you to interpret results."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": ["claudecode", "openclaude", "opencode", "codex", "qoder"],
                    "description": "Which CLI coding agent to bind the IM session to."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory for the new session; defaults to the agent workspace."
                }
            },
            "required": ["agent"]
        }),
    });
    tools.push(ToolDef {
        name: "cap_unbind_sticky".to_owned(),
        description: "Release any sticky cap binding on the current IM session and tear down \
            the underlying driver. Call this when the user signals they're done with the \
            coding agent and want to talk to the main LLM again: \"回到正常对话\", \
            \"stop using claudecode\", \"unbind\", \"done with codex\", \"thanks, that's all\". \
            Safe to call defensively — returns `status: \"not_bound\"` if nothing was bound."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {}
        }),
    });
    tools.push(ToolDef {
        name: "cap".to_owned(),
        description: "Dispatch a coding task to a CLI coding agent. Pick one of five agents via `agent`.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": ["claudecode", "openclaude", "opencode", "codex", "qoder"],
                    "description": "claudecode — Anthropic Claude Code (general-purpose, strongest tool use). openclaude — OpenClaude (Claude-compatible OSS fork). opencode — OpenCode (TUI-native, fast iteration). codex — OpenAI Codex (reasoning-heavy, slower). qoder — Qoder CLI (Claude Code compatible)."
                },
                "task":  { "type": "string", "description": "Task prompt for the agent." },
                "cwd":   { "type": "string", "description": "Optional working directory; defaults to the agent workspace." }
            },
            "required": ["agent", "task"]
        }),
    });
    // A2A v1.0 INPUT_REQUIRED / AUTH_REQUIRED suspend-resume bridge.
    // Calling this on a non-A2A turn returns an error result (no hang);
    // the LLM should fall back to a plain reply in that case.
    tools.push(ToolDef {
        name: "wait_input".to_owned(),
        description:
            "Pause the turn and wait for the calling A2A client to supply additional \
             input (a missing parameter, a confirmation, etc.). Publishes a \
             TASK_STATE_INPUT_REQUIRED (or TASK_STATE_AUTH_REQUIRED when \
             auth=true) status event, suspends until the client re-sends with the \
             same taskId, and returns the new text as `input`. Only meaningful on \
             A2A turns — returns an error on other channels."
                .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Human-readable prompt the client should render."
                },
                "auth": {
                    "type": "boolean",
                    "description": "If true, publish AUTH_REQUIRED (for credentials / OAuth handoff) instead of INPUT_REQUIRED. Defaults to false."
                }
            },
            "required": ["prompt"]
        }),
    });

    tools.push(ToolDef {
        name: "channel".to_owned(),
        description: "Perform channel-specific actions (send, reply, pin, delete messages). Channel is auto-detected from current session or can be specified explicitly: telegram, discord, slack, whatsapp, feishu, weixin, qq, dingtalk.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":    {"type": "string", "enum": ["send", "reply", "forward", "pin", "unpin", "delete"], "description": "Action to perform"},
                "channel":   {"type": "string", "description": "Channel type (auto-detected if omitted): telegram, discord, slack, whatsapp, feishu, weixin, qq, dingtalk"},
                "chatId":    {"type": "string", "description": "Chat/channel ID"},
                "text":      {"type": "string", "description": "Message text"},
                "messageId": {"type": "string", "description": "Message ID (for reply/pin/delete)"}
            },
            "required": ["action"]
        }),
    });

    tools.push(ToolDef {
        name: "anycli".to_owned(),
        description: "Extract structured data from websites using declarative adapters.\n\
            Actions:\n\
            - run: Execute an adapter command (e.g., hackernews top, bilibili hot)\n\
            - list: List all available adapters\n\
            - info: Show adapter details and available commands\n\
            - search: Search community hub for adapters\n\
            - install: Install an adapter from the hub\n\
            Built-in adapters: hackernews, bilibili, arxiv, wikipedia, github-trending.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["run", "list", "info", "search", "install"], "description": "Action to perform"},
                "adapter": {"type": "string", "description": "Adapter name (for run/info)"},
                "command": {"type": "string", "description": "Command name within adapter (for run)"},
                "params":  {"type": "object", "description": "Key-value parameters (for run), e.g. {\"limit\": \"10\", \"query\": \"rust\"}"},
                "query":   {"type": "string", "description": "Search query (for search)"},
                "name":    {"type": "string", "description": "Adapter name (for install)"},
                "format":  {"type": "string", "enum": ["json", "table", "csv", "markdown"], "description": "Output format (for run, default: json)"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "clarify".to_owned(),
        description: "Ask the user a clarifying question before proceeding. Use when:\n\
            - The request is ambiguous and multiple valid interpretations exist\n\
            - A choice is needed (e.g., which file, which format, which approach)\n\
            - Destructive or irreversible action needs confirmation\n\
            Provide options for quick selection or leave open-ended for free-form answers.\n\
            IMPORTANT: Do NOT use this for simple confirmations. Only when genuine ambiguity exists.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "The question to ask the user"},
                "options":  {"type": "array", "items": {"type": "string"}, "description": "Optional list of choices. Omit for open-ended questions."}
            },
            "required": ["question"]
        }),
    });
    tools.push(ToolDef {
        name: "pairing".to_owned(),
        description: "Manage channel pairing (dmPolicy=pairing). Actions: list (show pending codes and approved peers), approve (approve a pairing code), revoke (revoke an approved peer).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["list", "approve", "revoke"], "description": "Action to perform"},
                "code":    {"type": "string", "description": "Pairing code to approve (for approve action, e.g. ZGTB-NB79)"},
                "channel": {"type": "string", "description": "Channel name (for revoke action, e.g. qq, telegram)"},
                "peerId":  {"type": "string", "description": "Peer ID to revoke (for revoke action)"}
            },
            "required": ["action"]
        }),
    });

    // Document tools — split into simple independent tools for better small-model
    // compatibility. Formatting note injected into content-bearing tools.
    let doc_fmt_hint = " Structure content professionally: use # headings, - bullet lists, blank lines between sections. For notices/reports: add title, organize into sections.";

    tools.push(ToolDef {
        name: "create_docx".to_owned(),
        description: format!("Create a Word document (.docx).{doc_fmt_hint} After creating, use send_file to deliver."),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "File path, e.g. 'report.docx'"},
                "content": {"type": "string", "description": "Document content. Use # for headings, - for lists, blank lines for paragraphs."},
                "title":   {"type": "string", "description": "Document title (optional, displayed at top)"},
                "explanation": {"type": "string", "description": "One line: what you are creating and why."}
            },
            "required": ["path", "content"]
        }),
    });
    tools.push(ToolDef {
        name: "create_pdf".to_owned(),
        description: format!("Create a PDF document.{doc_fmt_hint} After creating, use send_file to deliver."),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "File path, e.g. 'report.pdf'"},
                "content": {"type": "string", "description": "Document content. Use # for headings, - for lists, blank lines for paragraphs."},
                "title":   {"type": "string", "description": "Document title (optional, displayed at top)"},
                "explanation": {"type": "string", "description": "One line: what you are creating and why."}
            },
            "required": ["path", "content"]
        }),
    });
    tools.push(ToolDef {
        name: "create_xlsx".to_owned(),
        description: "Create an Excel spreadsheet (.xlsx). Extract structured data into columns with meaningful headers. After creating, use send_file to deliver.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":   {"type": "string", "description": "File path, e.g. 'data.xlsx'"},
                "sheets": {"type": "array", "description": "Sheets: [{name, headers: [str], rows: [[value]]}]",
                    "items": {"type": "object", "properties": {
                        "name":    {"type": "string", "description": "Sheet name (tab label in the spreadsheet)."},
                        "headers": {"type": "array", "items": {"type": "string"}, "description": "Column header labels for the first row."},
                        "rows":    {"type": "array", "items": {"type": "array"}, "description": "Data rows, each an array of cell values in column order."}
                    }}
                },
                "explanation": {"type": "string", "description": "One line: what you are creating and why."}
            },
            "required": ["path", "sheets"]
        }),
    });
    tools.push(ToolDef {
        name: "create_pptx".to_owned(),
        description: "Create a PowerPoint presentation (.pptx). After creating, use send_file to deliver.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":   {"type": "string", "description": "File path, e.g. 'deck.pptx'"},
                "slides": {"type": "array", "description": "Slides: [{title, body}]",
                    "items": {"type": "object", "properties": {
                        "title": {"type": "string", "description": "Slide title displayed at the top."},
                        "body":  {"type": "string", "description": "Slide body text. Use newlines to separate bullet points."}
                    }}
                },
                "explanation": {"type": "string", "description": "One line: what you are creating and why."}
            },
            "required": ["path", "slides"]
        }),
    });
    // Keep doc tool for read/edit operations (less frequently used by small
    // models).
    tools.push(ToolDef {
        name: "doc".to_owned(),
        description: "Read or edit existing documents.\n\
            Actions: read_doc (xlsx/docx/pdf), edit_excel, edit_word, edit_pdf.\n\
            For CREATING new documents, use create_docx/create_pdf/create_xlsx/create_pptx instead.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["read_doc", "edit_excel", "edit_word", "edit_pdf"], "description": "Action to perform"},
                "path":    {"type": "string", "description": "File path"},
                "content": {"type": "string", "description": "For edit_word: replacement content"},
                "append":  {"type": "string", "description": "For edit_word: text to append"},
                "sheets":  {"type": "array", "description": "For edit_excel: [{name, headers, rows}]",
                    "items": {"type": "object", "properties": {
                        "name":    {"type": "string", "description": "Sheet name (tab label in the spreadsheet)."},
                        "headers": {"type": "array", "items": {"type": "string"}, "description": "Column header labels for the first row."},
                        "rows":    {"type": "array", "items": {"type": "array"}, "description": "Data rows, each an array of cell values in column order."}
                    }}
                },
                "append_rows": {"type": "array", "description": "For edit_excel: append rows to an existing sheet without replacing it.",
                    "items": {"type": "object", "properties": {
                        "sheet": {"type": "string", "description": "Name of the existing sheet to append to."},
                        "rows":  {"type": "array", "items": {"type": "array"}, "description": "Rows to append, each an array of cell values."}
                    }}
                },
                "replacements": {"type": "array", "description": "For edit_pdf: [{find, replace}]",
                    "items": {"type": "object", "properties": {
                        "find":    {"type": "string", "description": "Text string to find in the PDF."},
                        "replace": {"type": "string", "description": "Replacement text."}
                    }}
                },
                "delete_pages": {"type": "array", "description": "For edit_pdf: 1-indexed page numbers to delete", "items": {"type": "integer"}}
            },
            "required": ["action", "path"]
        }),
    });

    // Dynamic per-agent A2A tools.
    if let Some(reg) = agents {
        for handle in reg.all() {
            if handle.id == caller_id {
                continue;
            }
            tools.push(ToolDef {
                name: format!("agent_{}", handle.id),
                // Honor a per-agent `description` (capabilities + trigger
                // keywords) so a routing agent knows WHEN to delegate here.
                // Without it the generic blurb is opaque and the model just
                // does the work itself instead of dispatching. Mirrors the
                // external a2a-peer path below.
                description: handle.config.description.clone().unwrap_or_else(|| {
                    format!(
                        "Send a task to agent '{}'. Returns the agent's reply.",
                        handle.id
                    )
                }),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "Task or message to send"}
                    },
                    "required": ["text"]
                }),
            });
        }
    }

    // External remote agent A2A tools (remote gateways). Honor a
    // per-peer `description` so operators can spell out the peer's
    // capabilities + trigger keywords; small models (Qwen3.5-9B-class)
    // otherwise can't tell what `agent_spoke_aihub` does from the
    // generic auto-generated blurb and will refuse to route there.
    tracing::debug!(count = a2a_peers.len(), "build_tool_list: external agents");
    for ext in a2a_peers {
        if ext.id == caller_id {
            continue;
        }
        let description = ext.description.clone().unwrap_or_else(|| {
            format!(
                "Send a task to remote agent '{}' at {}. Returns the agent's reply.",
                ext.id, ext.url
            )
        });
        tools.push(ToolDef {
            name: format!("agent_{}", ext.id),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Task or message to send"}
                },
                "required": ["text"]
            }),
        });
    }

    // Skill tools.
    for skill in skills.all() {
        for spec in &skill.tools {
            tools.push(ToolDef {
                name: format!("{}.{}", skill.name, spec.name),
                description: spec.description.clone(),
                parameters: spec
                    .input_schema
                    .clone()
                    .unwrap_or_else(|| Value::Object(Default::default())),
            });
        }
    }

    // Inject `additionalProperties: false` and `$schema` into every tool's
    // parameters object. This enables constrained decoding in Ollama/vLLM,
    // which dramatically reduces digit-loss on small models (9b).
    for tool in &mut tools {
        if let Some(obj) = tool.parameters.as_object_mut() {
            obj.entry("additionalProperties").or_insert(json!(false));
            obj.entry("$schema")
                .or_insert(json!("http://json-schema.org/draft-07/schema#"));
        }
    }

    tools
}

/// Build the Windows `shell` tool description with PowerShell-edition-aware
/// language guidance (5.1 vs 7+). Detection is cached for the process
/// lifetime via `platform::detect_powershell_edition`.
///
/// The 5.1 / 7+ branches differ sharply (`&&`, `??`, ternary, `2>&1`
/// semantics, file encoding defaults). Steering the model with the right
/// syntax up front avoids a class of parser-error retries.
/// Windows shell guidance for the per-session `user_system` "Platform"
/// section (NOT the cached `shell` tool description — that's OS-agnostic).
/// Lives here next to the shell tool for cohesion, but is consumed by
/// `prompt_builder::build_user_system`. Reflects the actual host's
/// PowerShell edition via runtime detection, so it must stay in the
/// per-session layer that never enters the base-layer prefix hash.
pub(crate) fn windows_shell_guidance() -> String {
    use super::platform::{PowerShellEdition, detect_powershell_edition};

    let edition_section = match detect_powershell_edition() {
        Some(PowerShellEdition::Core) => {
            "\
             PowerShell edition detected: PowerShell 7+ (pwsh).\n\
             - Pipeline chain operators `&&` and `||` ARE available and work like bash. Prefer `cmd1 && cmd2` over `cmd1; cmd2` when cmd2 should only run if cmd1 succeeds.\n\
             - Ternary `$cond ? $a : $b`, null-coalescing `??`, and null-conditional `?.` ARE available.\n\
             - Default file encoding is UTF-8 without BOM.\n\
             \n"
        }
        Some(PowerShellEdition::Desktop) => {
            "\
             PowerShell edition detected: Windows PowerShell 5.1 (powershell.exe).\n\
             - Pipeline chain operators `&&` and `||` are NOT available — they cause a parser error. To run B only if A succeeds: `A; if ($?) { B }`. To chain unconditionally: `A; B`.\n\
             - Ternary (`?:`), null-coalescing (`??`), null-conditional (`?.`) operators are NOT available. Use `if/else` and explicit `$null -eq` checks.\n\
             - Avoid `2>&1` on native executables. In 5.1, redirecting a native command's stderr inside PowerShell wraps each line in an ErrorRecord and sets `$?` to `$false` even when the exe returned exit code 0. stderr is already captured for you — don't redirect it.\n\
             - Default file encoding is UTF-16 LE with BOM. When writing files other tools will read, pass `-Encoding utf8` to `Out-File`/`Set-Content`.\n\
             - `ConvertFrom-Json` returns a PSCustomObject, not a hashtable. `-AsHashtable` is not available.\n\
             \n"
        }
        None => {
            "\
             PowerShell edition not detected — assume Windows PowerShell 5.1 for compatibility.\n\
             - Do NOT use `&&`, `||`, ternary `?:`, null-coalescing `??`, or null-conditional `?.`. These are PowerShell 7+ only and parser-error on 5.1.\n\
             - To chain conditionally: `A; if ($?) { B }`. Unconditionally: `A; B`.\n\
             - When writing files other tools will read, pass `-Encoding utf8` to `Out-File`/`Set-Content` (5.1 default is UTF-16 LE BOM).\n\
             \n"
        }
    };

    format!(
        "Platform: Windows. Shell: PowerShell. Package manager: winget/scoop/choco (npm/pip for language deps).\n\
         IMPORTANT: For file listing use `list_dir`, for file search use `search_file`, for content search use `search_content`, for tool install use `install_tool`, for HTTP/API requests use `web_fetch`. Only use shell for commands that have no dedicated tool.\n\
         Use shell for: git operations, running scripts (node/python/cargo), system info (systeminfo, ipconfig, Get-Process), package management (npm/pip), process management (Start-Process, Stop-Process, taskkill).\n\
         Do NOT use shell for HTTP requests (curl/wget/Invoke-WebRequest) or file downloads — use `web_fetch` / `web_download` instead.\n\
         \n\
         {edition_section}\
         PowerShell language notes (apply to all editions):\n\
         - Variables prefix `$`; escape char is backtick `` ` `` (not `\\`).\n\
         - Verb-Noun cmdlet naming: Get-ChildItem, Set-Location, New-Item, Remove-Item.\n\
         - Env vars: read via `$env:NAME`, set via `$env:NAME = \"value\"` (NOT `Set-Variable`, NOT bash `export`).\n\
         - Registry: use `HKLM:\\SOFTWARE\\...`, `HKCU:\\...` (PSDrive prefix), NOT raw `HKEY_LOCAL_MACHINE\\...`.\n\
         - Call native exe with spaces in path via call operator: `& \"C:\\Program Files\\App\\app.exe\" arg1 arg2`.\n\
         - For args containing `-` / `@` / other PS metachars: use `--%` stop-parsing token, e.g. `git log --% --format=%H`.\n\
         - Multiline strings to native exes (commit messages, file content): use single-quoted here-string `@'...'@` — closing `'@` MUST be at column 0 (no leading whitespace) or you get a parse error.\n\
         \n\
         Interactive commands hang under -NonInteractive — NEVER use:\n\
         - `Read-Host`, `Get-Credential`, `Out-GridView`, `$Host.UI.PromptForChoice`, `pause`\n\
         - Destructive cmdlets that may prompt — add `-Confirm:$false` when you intend the action to proceed; `-Force` for read-only/hidden items.\n\
         - `git rebase -i`, `git add -i`, or anything that opens an interactive editor.\n\
         \n\
         Tool selection: PowerShell for file/system ops; python for data processing (CSV/JSON/automation).\n\
         Check tool availability first (`Get-Command python`, `Get-Command node`). Use `install_tool` for system tools.\n\
         \n\
         PowerShell patterns:\n\
         - Pipes: Get-Process | Sort-Object CPU -Descending | Select-Object -First 10\n\
         - Network connectivity check (not fetch): Test-NetConnection host -Port 80\n\
         - Text: (Get-Content file) -replace 'old','new'\n\
         - Dates: Get-Date -Format 'yyyy-MM-dd'; [DateTimeOffset]::Now.ToUnixTimeSeconds()\n\
         Python patterns: `python -c \"import json; ...\"` for one-liners, write to $env:TEMP\\script.py for multi-line.\n\
         \n\
         Best practices: Do NOT wrap commands in extra cmd /c or powershell -Command layers. Use `| Select-Object -First 10` to limit output.\n\
         Do NOT use shell for destructive operations on personal directories (Desktop, Downloads, Documents).\n\
         Commands run in background by default (wait=false). Use wait=true only for short commands where you need the output immediately.\n\
         If a command fails, do NOT retry with the same arguments. Try a different approach or ask the user.\n\
         \n\
         Multiple commands:\n\
         - Independent → emit MULTIPLE shell tool calls in ONE message for parallelism.\n\
         - Dependent → single shell call with edition-appropriate chaining (see edition section above).\n\
         - Use `;` only when you don't care whether earlier commands succeed.\n\
         - Do NOT use newlines to separate commands.\n\
         \n\
         Sleep is rarely the right answer:\n\
         - Don't sleep between commands that can run back-to-back.\n\
         - Don't sleep-poll a background task you started with `wait=false` — task results are delivered on your next turn automatically.\n\
         - Don't sleep-retry a failing command — find the actual cause and change something.\n\
         \n\
         Git safety:\n\
         - NEVER force-push to main / master.\n\
         - NEVER `--no-verify`, `--no-gpg-sign`, or `-c commit.gpgsign=false` unless the user explicitly asks — fix the hook failure instead.\n\
         - AVOID `git add -A` / `git add .` — stage specific files so you don't leak `.env`, credentials, or build artifacts.\n\
         - Prefer NEW commits over `--amend`; amending after a pre-commit-hook failure can destroy work because the failed commit never landed."
    )
}

#[cfg(test)]
mod plugin_catalog_tests {
    use super::*;
    use crate::registry::AgentRegistry;
    use rsclaw_config::schema::A2aPeerConfig;
    use rsclaw_skill::SkillRegistry;

    #[test]
    fn cold_tools_all_exist_and_stub_lists_them() {
        // Every COLD_TOOLS entry must name a real builtin — a renamed or
        // removed tool silently un-deferring (or a stub advertising a tool
        // that no longer exists) is exactly the drift this guards against.
        let skills = SkillRegistry::new();
        let all = build_tool_list(&skills, None, "main", &[]);
        let names: std::collections::HashSet<&str> =
            all.iter().map(|t| t.name.as_str()).collect();
        for cold in COLD_TOOLS {
            assert!(names.contains(cold), "COLD_TOOLS entry `{cold}` is not a builtin tool");
        }
        let entries: Vec<(String, String)> = COLD_TOOLS
            .iter()
            .map(|n| ((*n).to_owned(), (*n).to_owned()))
            .collect();
        let stub = request_tool_def(&entries);
        assert_eq!(stub.name, "request_tool");
        for cold in COLD_TOOLS {
            assert!(stub.description.contains(cold), "stub must list `{cold}`");
        }
    }

    #[test]
    fn skill_list_schema_supports_filter_and_pagination() {
        let tools = build_tool_list(
            &SkillRegistry::default(),
            None::<&AgentRegistry>,
            "main",
            &[] as &[A2aPeerConfig],
        );
        let skill_list = tools
            .iter()
            .find(|tool| tool.name == "skill_list")
            .expect("skill_list tool");

        assert!(skill_list.parameters["properties"].get("query").is_some());
        assert!(skill_list.parameters["properties"].get("limit").is_some());
        assert!(skill_list.parameters["properties"].get("offset").is_some());
        assert!(skill_list.description.contains("query"));
        assert!(skill_list.description.contains("offset"));
        assert!(skill_list.description.contains("Do not use shell"));
    }

    #[test]
    fn plugin_meta_tools_are_advertised_as_consolidated_entrypoints() {
        let tools = build_tool_list(
            &SkillRegistry::default(),
            None::<&AgentRegistry>,
            "main",
            &[] as &[A2aPeerConfig],
        );
        let names: std::collections::HashSet<_> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains("plugin_list"));
        assert!(names.contains("plugin_search"));
        assert!(names.contains("plugin_describe"));
        assert!(names.contains("plugin_invoke"));
    }

    #[test]
    fn custom_tools_override_toolset_preset() {
        // Hub-router semantic: `toolset: "minimal"` + explicit `tools: [...]`
        // must yield ONLY the explicit list, never the minimal base. Pre-fix
        // this merged base ∪ custom, exposing `shell` to a router agent and
        // making 9B models hallucinate `pip install` instead of routing.
        let custom = vec!["agent_spoke_aihub".to_string(), "memory".to_string()];
        let names = toolset_allowed_names("minimal", Some(&custom))
            .expect("explicit custom must produce whitelist");
        assert_eq!(names.len(), 2, "must NOT include minimal base");
        assert!(names.contains("agent_spoke_aihub"));
        assert!(names.contains("memory"));
        assert!(!names.contains("shell"), "shell from minimal must be excluded");
        assert!(!names.contains("read_file"), "read_file from minimal must be excluded");
    }

    #[test]
    fn custom_tools_override_works_for_every_preset() {
        let custom = vec!["only_me".to_string()];
        for preset in ["minimal", "web", "code", "standard", "full"] {
            let names = toolset_allowed_names(preset, Some(&custom))
                .expect("override produces whitelist");
            assert_eq!(names.len(), 1, "preset={preset}");
            assert!(names.contains("only_me"), "preset={preset}");
        }
    }

    #[test]
    fn preset_alone_still_returns_preset_base() {
        // Regression guard: when `tools` is None, the preset behavior is
        // unchanged.
        let names = toolset_allowed_names("minimal", None).expect("base");
        assert!(names.contains("shell"));
        assert!(names.contains("read_file"));
    }

    #[test]
    fn plugin_meta_tools_are_available_in_filtered_toolsets() {
        for toolset in ["minimal", "web", "code", "standard"] {
            let names = toolset_allowed_names(toolset, None).expect("filtered toolset");

            assert!(names.contains("plugin_list"), "{toolset}");
            assert!(names.contains("plugin_search"), "{toolset}");
            assert!(names.contains("plugin_describe"), "{toolset}");
            assert!(names.contains("plugin_invoke"), "{toolset}");
        }
    }

    #[test]
    fn catalog_lists_common_tools_first_with_overflow() {
        // (name, description) pairs; "publish"/"list" are common.
        let tools = vec![
            ("zeta", "z"),
            ("publish", "Publish a video"),
            ("list", "List items"),
            ("alpha", "a"),
        ];
        let common = vec!["publish".to_string(), "list".to_string()];
        let out = plugin_tool_list(&tools, &common, 3);
        // Common first, in commonTools order; then others by name; capped at 3.
        assert_eq!(
            out,
            "- publish: Publish a video\n- list: List items\n- alpha: a\n- …1 more tool — plugin_search to find them"
        );

        // No common declared → first N by name + overflow.
        let none: Vec<String> = vec![];
        let out2 = plugin_tool_list(&tools, &none, 2);
        assert_eq!(
            out2,
            "- alpha: a\n- list: List items\n- …2 more tools — plugin_search to find them"
        );

        // Empty.
        assert_eq!(plugin_tool_list(&[], &none, 5), "  (no declared tools)");
    }

    #[test]
    fn catalog_block_is_skill_md_shaped() {
        let tools = vec![("publish", "Publish a video")];
        let block = render_plugin_catalog_block("douyin", "0.1.0", "Douyin ops", &tools, &[], 5);
        assert!(block.contains("### douyin — Douyin ops"));
        assert!(block.contains("- publish: Publish a video"));
        assert!(block.contains("plugin_search"));

        // Empty blurb → no dangling em-dash.
        let bare = render_plugin_catalog_block("p", "1.0", "", &tools, &[], 5);
        assert!(bare.contains("### p (v1.0)"));
        assert!(!bare.contains("—"));
    }
}
