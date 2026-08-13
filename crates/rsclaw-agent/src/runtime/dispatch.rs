//! Tool dispatch — routes tool calls to implementations.

use anyhow::Context;
use futures::StreamExt;
use rsclaw_skill::{RunOptions, run_tool};

use super::*;

impl AgentRuntime {
    pub(super) fn allowed_tools_for_dispatch(&self) -> Option<std::collections::HashSet<String>> {
        let model_cfg = self.handle.config.model.as_ref()?;
        let explicit_toolset = model_cfg.toolset.as_deref();
        let custom_tools = model_cfg.tools.as_ref();
        if explicit_toolset.is_none() && custom_tools.is_none() {
            return None;
        }
        crate::tools_builder::toolset_allowed_names(
            explicit_toolset.unwrap_or("standard"),
            custom_tools,
        )
    }

    pub(super) fn has_stock_tool_provider(&self) -> bool {
        self.wasm_plugins.iter().any(|wp| {
            wp.capabilities.iter().any(|c| c == "trustedToolAlias")
                && wp
                    .tool_aliases
                    .values()
                    .any(|alias| is_stock_tool_name(alias))
        })
    }

    pub(super) async fn dispatch_tool(
        &self,
        ctx: &RunContext,
        _id: &str,
        raw_name: &str,
        args: Value,
    ) -> Result<Value> {
        // Tolerate `plugin=search_tools` (instead of the canonical
        // `plugin_search`) — observed from rsclaw-agent-v1 on the 4070
        // fleet, model treats the namespace separator as `key=value`.
        // Rewriting `=` to `.` lets the dispatch's legacy-dotted aliases
        // (kept below for backward compat) still resolve. We do NOT alias
        // `plugin.list` → `plugin_list` because the semantics differ:
        // `plugin_list` enumerates installed plugins, while a model
        // emitting `plugin.list` more commonly wants to enumerate one
        // plugin's tools (which is `plugin_search {plugin: …}` in browse
        // mode). Let the "unknown tool" error surface so the model
        // re-reads the catalog and self-corrects (logs show it does).
        let normalized: String = if raw_name.contains('=') && !raw_name.contains('.') {
            raw_name.replacen('=', ".", 1)
        } else {
            raw_name.to_owned()
        };
        let name = normalized.as_str();

        // Whitelist enforcement: build_tool_list hides tools outside the
        // configured `toolset` / `tools[]` from the LLM, but the dispatcher
        // was matching every tool name unconditionally. A small model that
        // hallucinated a familiar name (`video_gen`, `web_browser`, …) would
        // still bypass the gate and trigger the real implementation. For the
        // hub-router pattern (toolset: "minimal", tools: ["agent_spoke_mac",
        // ...]) this meant the model occasionally chose a hub-side tool
        // instead of routing to the spoke. Enforce here whenever the agent
        // config explicitly limits its toolset.
        if let Some(allowed) = self.allowed_tools_for_dispatch() {
            if !allowed.contains(name) {
                return Err(anyhow!(
                    "tool '{name}' is not in this agent's whitelist — \
                     hallucinated tool names are blocked. Available: {}",
                    {
                        let mut v: Vec<&str> = allowed.iter().map(String::as_str).collect();
                        v.sort();
                        v.join(", ")
                    }
                ));
            }
        }

        // Per-session filter: even when no explicit whitelist is configured,
        // run_turn strips certain tools for special sessions (cap-followup
        // bans research/exec/chain tools so it can briefly summarise without
        // wandering off). The model still occasionally hallucinates a stripped
        // name from training data — re-apply the same filter at dispatch.
        if ctx.session_key.ends_with(":cap-followup") {
            const FOLLOWUP_BLOCKED: &[&str] = &[
                "web_search",
                "web_fetch",
                "browser",
                "computer_use",
                "shell",
                "execute_command",
                "exec",
                "cap",
                "cap_live",
                "cap_live_end",
                "cap_bind_sticky",
                "cap_unbind_sticky",
                "task",
            ];
            if FOLLOWUP_BLOCKED.contains(&name) {
                return Err(anyhow!(
                    "tool '{name}' is unavailable in cap-followup sessions — \
                     just summarise the completed cap results in plain text."
                ));
            }
        }

        if is_stock_tool_name(name) && !self.has_stock_tool_provider() {
            return Err(anyhow!(
                "tool '{name}' is unavailable: no trusted stock WASM plugin has claimed this tool alias"
            ));
        }

        if let Some((wp, plugin_tool)) = self.wasm_plugins.iter().find_map(|wp| {
            if !wp.capabilities.iter().any(|c| c == "trustedToolAlias") {
                return None;
            }
            wp.tool_aliases
                .iter()
                .find(|(_, alias)| alias.as_str() == name)
                .map(|(plugin_tool, _)| (wp, plugin_tool.as_str()))
        }) {
            let notify_ctx = self.notification_tx.as_ref().map(|tx| {
                rsclaw_plugin::wasm_runtime::WasmNotifyCtx {
                    tx: tx.clone(),
                    target_id: if !ctx.chat_id.is_empty() {
                        ctx.chat_id.clone()
                    } else {
                        ctx.peer_id.clone()
                    },
                    channel: ctx.channel.clone(),
                    agent_id: ctx.agent_id.clone(),
                    peer_id: ctx.peer_id.clone(),
                    chat_id: ctx.chat_id.clone(),
                    session_key: ctx.session_key.clone(),
                    is_group: false,
                    account: ctx.account.clone(),
                }
            });
            return wp.call_tool_with_ctx(plugin_tool, args, notify_ctx).await;
        }

        // 2. Built-in tools (checked before A2A prefix so reserved names are not
        //    hijacked).
        match name {
            // --- Consolidated tools (new unified names) ---
            "memory" => return self.tool_memory_consolidated(ctx, args).await,
            "todo" => return self.tool_todo(ctx, args).await,
            "session" => return self.tool_session_consolidated(ctx, args).await,
            "agent" | "subagents" => return self.tool_agent_consolidated(ctx, args).await,
            "channel" => return self.tool_channel_consolidated(args).await,

            // A2A v1.0 INPUT_REQUIRED / AUTH_REQUIRED suspend-resume bridge.
            // When the LLM calls this tool the runtime publishes
            // TASK_STATE_INPUT_REQUIRED (or AUTH_REQUIRED), registers a
            // resume handle on `state.suspended_tasks`, and awaits the
            // client's next SendMessage on the same taskId — which the
            // dispatcher routes through the resume short-path. The new
            // text becomes this tool's return value, and the agent loop
            // continues with that text as a fresh tool result. No-op on
            // non-A2A turns: returns an error instead of hanging the loop.
            "wait_input" => return self.tool_wait_input(ctx, args).await,
            "wait_auth" => return self.tool_wait_input(ctx, inject_auth(args)).await,

            // --- Backward compat: old names map to consolidated handlers ---
            "memory_search" => {
                return self
                    .tool_memory_consolidated(ctx, inject_action(args, "search"))
                    .await;
            }
            "memory_get" => {
                return self
                    .tool_memory_consolidated(ctx, inject_action(args, "get"))
                    .await;
            }
            "memory_put" => {
                return self
                    .tool_memory_consolidated(ctx, inject_memory_put_compat(args))
                    .await;
            }
            "memory_delete" => {
                return self
                    .tool_memory_consolidated(ctx, inject_action(args, "delete"))
                    .await;
            }
            "sessions_send" => {
                return self
                    .tool_session_consolidated(ctx, inject_action(args, "send"))
                    .await;
            }
            "sessions_list" => {
                return self
                    .tool_session_consolidated(ctx, inject_action(args, "list"))
                    .await;
            }
            "sessions_history" => {
                return self
                    .tool_session_consolidated(ctx, inject_action(args, "history"))
                    .await;
            }
            "session_status" => {
                return self
                    .tool_session_consolidated(ctx, inject_action(args, "status"))
                    .await;
            }
            "agent_create" => {
                return self
                    .tool_agent_consolidated(ctx, inject_action(args, "create"))
                    .await;
            }
            "agent_list" | "agents_list" => {
                return self
                    .tool_agent_consolidated(ctx, inject_action(args, "list"))
                    .await;
            }
            "telegram_actions" => {
                return self
                    .tool_channel_consolidated(inject_channel(args, "telegram"))
                    .await;
            }
            "discord_actions" => {
                return self
                    .tool_channel_consolidated(inject_channel(args, "discord"))
                    .await;
            }
            "slack_actions" => {
                return self
                    .tool_channel_consolidated(inject_channel(args, "slack"))
                    .await;
            }
            "whatsapp_actions" => {
                return self
                    .tool_channel_consolidated(inject_channel(args, "whatsapp"))
                    .await;
            }
            "feishu_actions" => {
                return self
                    .tool_channel_consolidated(inject_channel(args, "feishu"))
                    .await;
            }
            "weixin_actions" => {
                return self
                    .tool_channel_consolidated(inject_channel(args, "wechat"))
                    .await;
            }
            "qq_actions" => {
                return self
                    .tool_channel_consolidated(inject_channel(args, "qq"))
                    .await;
            }
            "dingtalk_actions" => {
                return self
                    .tool_channel_consolidated(inject_channel(args, "dingtalk"))
                    .await;
            }

            // --- Standalone tools (unchanged) ---
            "send_file" => {
                // Returns a marker that the agent loop picks up to add to tool_files.
                let path = args["path"].as_str().unwrap_or("").to_owned();
                // In voice-reply mode the auto-TTS hook already attaches a
                // freshly-synthesised audio file to the reply. If the LLM
                // ALSO calls send_file with an audio path (often a stale
                // /tmp/rsclaw_tts_*.wav from an earlier turn), the user
                // receives two audio messages — usually with mismatched
                // durations, since the LLM picks an old file. Short-circuit
                // those calls and let auto-TTS own the audio channel.
                let lower_path = path.to_lowercase();
                let path_is_audio = lower_path.ends_with(".wav")
                    || lower_path.ends_with(".mp3")
                    || lower_path.ends_with(".ogg")
                    || lower_path.ends_with(".opus")
                    || lower_path.ends_with(".m4a")
                    || lower_path.ends_with(".aac")
                    || lower_path.ends_with(".flac")
                    || lower_path.ends_with(".silk")
                    || lower_path.ends_with(".amr");
                if path_is_audio && self.voice_mode_sessions.contains(&ctx.session_key) {
                    debug!(
                        session = %ctx.session_key,
                        path = %path,
                        "send_file: skipped audio attachment (voice_mode active, auto-TTS owns the audio)"
                    );
                    return Ok(json!({
                        "skipped": true,
                        "reason": "voice_mode active — auto-TTS will attach the audio reply; do not send separate audio files",
                        "path": path,
                    }));
                }
                let workspace = self
                    .handle
                    .config
                    .workspace
                    .as_deref()
                    .or(self.live.agents.read().await.defaults.workspace.as_deref())
                    .map(expand_tilde)
                    .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("workspace"));
                let pb = std::path::PathBuf::from(&path);
                let full = if pb.is_absolute() {
                    pb
                } else {
                    workspace.join(&path)
                };

                // Reuse the same safety checks as the read tool.
                if let Err(e) = check_read_safety(&path, &full) {
                    warn!("send_file: {e}");
                    return Ok(json!({"error": e.to_string()}));
                }

                if !full.exists() {
                    return Ok(json!({"error": format!("file not found: {}", full.display())}));
                }
                if let Ok(meta) = full.metadata() {
                    if meta.len() > 50_000_000 {
                        return Ok(json!({"error": "file too large (>50MB)"}));
                    }
                }
                let filename = full
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                return Ok(json!({
                    "__send_file": true,
                    "path": full.to_string_lossy(),
                    "filename": filename,
                    "size": full.metadata().map(|m| m.len()).unwrap_or(0),
                }));
            }
            "read_file" | "read" => return self.tool_read(args).await,
            "read_artifact" => return self.tool_read_artifact(ctx, args).await,
            "read_session_archive" => return self.tool_read_session_archive(ctx, args).await,
            "knowledge_base" | "kb_search" => return self.tool_knowledge_base(args).await,
            "stock_quote" | "stock_kline" | "stock_snapshot" | "stock_ask" | "stock_query"
            | "stock_chart" | "stock_watchlist" => {
                return Err(anyhow!(
                    "tool '{name}' is provided by the stock plugin alias layer, but no loaded plugin claimed this exact alias"
                ));
            }
            "research_ingest_wechat" => return self.tool_research_ingest_wechat(args).await,
            "research_analyze_charts" => return self.tool_research_analyze_charts(args).await,
            "write_file" | "write" => return self.tool_write(args).await,
            "edit_file" | "edit" => return self.tool_edit(args).await,
            "shell" | "execute_command" | "exec" => return self.tool_exec(ctx, _id, args).await,
            "skill_use" => return self.tool_use_skill(args),
            "skill_list" => return self.tool_skill_list(args),
            "skill_search" => return self.tool_skill_search(args).await,
            "skill_install" => return self.tool_skill_install(args).await,
            "skill_remove" => return self.tool_skill_remove(args).await,
            // Canonical (current): underscored, vendor-regex-compliant.
            // Legacy aliases kept for sessions whose model emits the old
            // dotted form from cache — dispatch still routes correctly,
            // but the names are NO LONGER advertised in tools / prompts.
            "plugin_list" | "plugin.info" | "plugin_info" => {
                return self.tool_plugin_info(args).await;
            }
            "plugin_search" | "plugin.search_tools" | "plugin_search_tools" => {
                return self.tool_plugin_search_tools(args).await;
            }
            "plugin_describe" | "plugin.describe_tool" | "plugin_describe_tool" => {
                return self.tool_plugin_describe_tool(args).await;
            }
            "plugin_invoke" | "plugin.invoke" => return self.tool_plugin_invoke(ctx, args).await,
            "task" => return self.tool_task(ctx, args).await,
            "task_finish" => return self.tool_task_finish(ctx, args).await,
            "ask_user" => return self.tool_ask_user(ctx, args).await,
            "install_tool" | "tool_install" => return self.tool_install(args).await,
            "list_dir" => return self.tool_list_dir(args).await,
            "search_file" => return self.tool_search_file(args).await,
            "search_content" => return self.tool_search_content(args).await,
            "web_search" => {
                // Inject the last user message so the query planner can work
                // with the original intent rather than the agent's rewritten query.
                let mut args = args;
                if args.get("_user_query").is_none() {
                    if let Some(msgs) = self.sessions.get(&ctx.session_key) {
                        if let Some(uq) = msgs
                            .iter()
                            .rev()
                            .find(|m| m.role == rsclaw_provider::Role::User)
                            .and_then(|m| match &m.content {
                                rsclaw_provider::MessageContent::Text(t) => Some(t.as_str()),
                                _ => None,
                            })
                        {
                            args["_user_query"] = serde_json::Value::String(uq.to_owned());
                        }
                    }
                }
                return self.tool_web_search(args).await;
            }
            "web_fetch" => return self.tool_web_fetch(ctx, args).await,
            "web_download" => return self.tool_web_download(args).await,
            "web_browser" | "browser" => return self.tool_web_browser(ctx, args).await,
            "computer_use" => return self.tool_computer_use(ctx, args).await,
            "image_gen" | "image" => return self.tool_image(args, ctx).await,
            "ocr" => return self.tool_ocr(args).await,
            "video_gen" | "video" => return self.tool_video(args, ctx).await,
            "avatar_gen" | "avatar" => return self.tool_avatar_gen(args, ctx).await,
            "mv_gen" | "mv" => return self.tool_mv_gen(args, ctx).await,
            "music_gen" | "music" => return self.tool_music(args).await,
            "voice_gen" | "voice" => return self.tool_voice(args).await,
            "pdf" => return self.tool_pdf(args).await,
            "text_to_voice" | "text_to_speech" | "tts" => return self.tool_tts(args).await,
            "send_message" | "message" => return self.tool_message(args).await,
            "anycli" | "opencli" => return self.tool_anycli(args).await,
            "request_tool" => {
                // v1 leaks trailing whitespace into string args — trim before lookup.
                let name = args["name"].as_str().unwrap_or("").trim().to_owned();
                // Plugin tool group: "<plugin>:<group>" (v2 toolGroups).
                if name.contains(':') {
                    let valid = name.split_once(':').is_some_and(|(pl, gr)| {
                        self.wasm_plugins.iter().any(|wp| {
                            wp.name == pl && wp.tools.iter().any(|t| t.group.as_deref() == Some(gr))
                        }) || self
                            .plugins
                            .as_deref()
                            .map(|reg| {
                                reg.js_plugins_iter().any(|(n, p)| {
                                    n == pl
                                        && p.manifest
                                            .tools
                                            .iter()
                                            .any(|t| t.group.as_deref() == Some(gr))
                                })
                            })
                            .unwrap_or(false)
                    });
                    if !valid {
                        return Ok(json!({
                            "error": format!("'{name}' is not a known plugin tool group"),
                            "hint": "Use a \"<plugin>:<group>\" entry exactly as listed in this tool's description."
                        }));
                    }
                    if let Ok(mut g) = self.handle.cold_enabled.write() {
                        g.entry(ctx.session_key.clone())
                            .or_default()
                            .insert(format!("pg:{name}"));
                    }
                    return Ok(json!({
                        "enabled": name,
                        "note": "Group tools are now available for this session — call them directly."
                    }));
                }
                if !crate::tools_builder::COLD_TOOLS.contains(&name.as_str()) {
                    return Ok(json!({
                        "error": format!("'{name}' is not a deferred tool"),
                        "deferred": crate::tools_builder::COLD_TOOLS,
                    }));
                }
                if let Ok(mut g) = self.handle.cold_enabled.write() {
                    g.entry(ctx.session_key.clone())
                        .or_default()
                        .insert(name.clone());
                }
                return Ok(json!({
                    "enabled": name,
                    "note": "Tool is now available for this session — call it directly."
                }));
            }
            "cron" => return self.tool_cron(args, ctx).await,
            "gateway" => return self.tool_gateway(args).await,
            "pairing" => return self.tool_pairing(args).await,
            "doc" => return self.tool_doc(args).await,
            "create_docx" => {
                let mut a = args.clone();
                a["action"] = serde_json::json!("create_word");
                return self.tool_doc(a).await;
            }
            "create_pdf" => {
                let mut a = args.clone();
                a["action"] = serde_json::json!("create_pdf");
                return self.tool_doc(a).await;
            }
            "create_xlsx" => {
                let mut a = args.clone();
                a["action"] = serde_json::json!("create_excel");
                return self.tool_doc(a).await;
            }
            "create_pptx" => {
                let mut a = args.clone();
                a["action"] = serde_json::json!("create_ppt");
                return self.tool_doc(a).await;
            }
            "cap" => return self.tool_cap(ctx, args).await,
            "cap_live" => return self.tool_cap_live(ctx, args).await,
            "cap_live_end" => return self.tool_cap_live_end(ctx, args).await,
            "cap_bind_sticky" => return self.tool_cap_bind_sticky(ctx, args).await,
            "cap_unbind_sticky" => return self.tool_cap_unbind_sticky(ctx, args).await,
            _ => {}
        }

        // 1. A2A: `agent_<id>` prefix → invoke another agent via registry.
        if let Some(agent_id) = name.strip_prefix("agent_") {
            return self.dispatch_a2a(ctx, agent_id, args).await;
        }

        // 3. MCP tool: prefixed with `mcp_<server>_`.
        if name.starts_with("mcp_") {
            if let Some(ref mcp) = self.mcp
                && let Some(client) = mcp.find_for_tool(name).await
            {
                // Strip the `mcp_<server>_` prefix to get the original tool name.
                let prefix = format!("mcp_{}_", client.name);
                let original_name = name.strip_prefix(&prefix).unwrap_or(name);
                let result = client.call_tool(original_name, args).await?;
                // MCP tools/call returns { content: [...] } — extract text.
                let text = result
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_else(|| result.to_string());
                return Ok(serde_json::json!(text));
            }
            return Err(anyhow!("MCP tool `{name}` not found"));
        }

        // 4. Plugin tool. Canonical wire form is `<plugin>__<tool>` (v1.9+,
        //    OpenAI-name-compatible). Legacy `<plugin>.<tool>` from older transcripts
        //    is still accepted by trying the new separator first and falling back. Wasm
        //    wins on collision; must precede skill match because plugins are higher in
        //    the priority ladder.
        if let Some((plugin_name, tool_name)) = name
            .split_once(PLUGIN_TOOL_SEP)
            .or_else(|| name.split_once('.'))
        {
            if let Some(wp) = self.wasm_plugins.iter().find(|p| p.name == plugin_name) {
                let notify_ctx = self.notification_tx.as_ref().map(|tx| {
                    rsclaw_plugin::wasm_runtime::WasmNotifyCtx {
                        tx: tx.clone(),
                        target_id: if !ctx.chat_id.is_empty() {
                            ctx.chat_id.clone()
                        } else {
                            ctx.peer_id.clone()
                        },
                        channel: ctx.channel.clone(),
                        agent_id: ctx.agent_id.clone(),
                        peer_id: ctx.peer_id.clone(),
                        chat_id: ctx.chat_id.clone(),
                        session_key: ctx.session_key.clone(),
                        is_group: false,
                        account: ctx.account.clone(),
                    }
                });
                return wp.call_tool_with_ctx(tool_name, args, notify_ctx).await;
            }
            // 4-bis. Shell plugin tool — same `<plugin>.<tool>` namespace; wasm
            //        wins on collision (above). The plugin spawns once at startup
            //        and we hand it the per-call ctx so it can dispatch host
            //        methods (notify, log, etc.) on the active conversation.
            if let Some(reg) = self.plugins.as_ref()
                && let Some(plugin) = reg.get_js(plugin_name)
            {
                let target_id = if !ctx.chat_id.is_empty() {
                    ctx.chat_id.clone()
                } else {
                    ctx.peer_id.clone()
                };
                let params = serde_json::json!({
                    "tool": tool_name,
                    "args": args,
                    "_ctx": {
                        "target_id":   target_id,
                        "channel":     ctx.channel.clone(),
                        "session_key": ctx.session_key.clone(),
                    }
                });
                return plugin.call("tool_call", params).await;
            }
        }

        // 5. Skill tool.
        let (skill_name, tool_name) = name.split_once('.').unwrap_or((name, name));
        let Some(skill) = self.skills.get(skill_name) else {
            return Err(anyhow!("unknown tool: `{name}`"));
        };
        // Find the matching tool spec within the skill.
        let Some(spec) = skill.tools.iter().find(|t| t.name == tool_name) else {
            return Err(anyhow!("skill `{}` has no tool `{tool_name}`", skill.name));
        };
        run_tool(spec, &skill.dir, args, &RunOptions::default()).await
    }

    // -----------------------------------------------------------------------
    // A2A dispatch
    // -----------------------------------------------------------------------

    pub(super) async fn dispatch_a2a(
        &self,
        ctx: &RunContext,
        agent_id: &str,
        args: Value,
    ) -> Result<Value> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("A2A: `text` argument required"))?
            .to_owned();

        // 1. Try local registry first.
        if let Some(ref registry) = self.agents
            && let Ok(target) = registry.get(agent_id)
        {
            // Derive a child session key so A2A calls have isolated context.
            let child_session = format!("{}:a2a:{agent_id}", ctx.session_key);

            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentReply>();
            let msg = AgentMessage {
                session_key: child_session,
                text,
                channel: format!("a2a:{}", ctx.agent_id),
                peer_id: ctx.agent_id.clone(),
                chat_id: String::new(),
                reply_tx,
                task_id: None,
                context_id: None,
                event_tx: None,
                cancel_token: None,
                input_request_tx: None,
                extra_tools: vec![],
                images: vec![],
                files: vec![],
                account: None,
            };

            target
                .tx
                .send(msg)
                .await
                .map_err(|_| anyhow!("A2A: agent `{agent_id}` inbox closed"))?;

            let a2a_timeout_secs =
                self.config
                    .agents
                    .defaults
                    .timeout_seconds
                    .unwrap_or(DEFAULT_TIMEOUT_SECONDS as u32) as u64;

            let reply = tokio::time::timeout(Duration::from_secs(a2a_timeout_secs), reply_rx)
                .await
                .map_err(|_| {
                    anyhow!("A2A: agent `{agent_id}` timed out after {a2a_timeout_secs}s")
                })?
                .map_err(|_| anyhow!("A2A: reply channel dropped"))?;

            return Ok(Value::String(reply.text));
        }

        // 2. Resolve the configured remote. Runtime-owned direct/relay routes
        // and HTTP/SSE fallback use the same peer declaration.
        // Normalize: LLMs sometimes replace _ with - in tool names.
        let normalized_id = agent_id.replace('-', "_");
        if let Some(ext) = self
            .config
            .agents
            .a2a
            .iter()
            .find(|e| e.id == agent_id || e.id == normalized_id)
        {
            // Prefer runtime-owned direct DataChannel, then authenticated hub
            // relay. No route (`Ok(None)`) falls through to HTTP/SSE below.
            if let (Some(host), Some(node_id)) =
                (rsclaw_types::outbound_a2a_host(), ext.node_id.as_deref())
            {
                let remote_id = ext.remote_agent_id.as_deref().unwrap_or("main");
                let target = format!("{node_id}/{remote_id}");
                if let Some(reply) = host
                    .try_send(rsclaw_types::OutboundA2aRequest {
                        target,
                        text: text.clone(),
                        context_id: ctx.session_key.clone(),
                        principal: ctx.agent_id.clone(),
                    })
                    .await
                    .with_context(|| format!("A2A runtime transport `{agent_id}`"))?
                {
                    return Ok(Value::String(reply));
                }
            }

            // 3. Fall back to the peer's public HTTP/SSE endpoint.
            use rsclaw_a2a_types::client::A2aClient;
            let client = A2aClient::new();
            // Use remote agent ID if configured, otherwise omit (uses remote default).
            let remote_id = ext.remote_agent_id.as_deref().unwrap_or("");
            // Resolve the peer token (plain / ${ENV}) at call time. File/exec
            // secret providers require runtime secrets config and are not exposed
            // across the agent/runtime dependency boundary.
            let peer_token = ext.auth_token.as_ref().and_then(|s| s.resolve_early());
            let stream = client
                .send_streaming_message(
                    &ext.url,
                    remote_id,
                    &text,
                    &ctx.session_key,
                    peer_token.as_deref(),
                )
                .await
                .map_err(|e| anyhow!("A2A remote `{agent_id}`: {e}"))?;
            tokio::pin!(stream);

            // Drain the SSE stream until the remote publishes a terminal
            // status event.
            //   - status-update with message text: forward to lead's channel so streaming
            //     subscribers see sub-agent progress in real time (e.g. "calling tool
            //     memory_search").
            //   - artifact-update text parts: accumulate as the reply text ultimately
            //     returned to the LLM.
            //   - final=true: terminate; FAILED captures the error message, CANCELED
            //     short-circuits with an error.
            //
            // Cancellation: if the lead's turn_ctx is cancelled, dropping the
            // pinned SSE stream closes the HTTP connection; the remote's SSE
            // handler observes the close and publishes a Canceled terminal
            // event, propagating cancel down to the sub-agent.
            let mut accumulated = String::new();
            let mut last_msg = String::new();
            let mut last_error: Option<String> = None;
            loop {
                let cancel_fut = async {
                    match &ctx.turn_ctx.cancel_token {
                        Some(t) => t.cancelled().await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancel_fut => {
                        return Err(anyhow!(
                            "A2A remote `{agent_id}`: cancelled by client"
                        ));
                    }
                    next = stream.next() => {
                        let Some(event) = next else { break; };
                        let event = event.map_err(|e| anyhow!(
                            "A2A remote `{agent_id}` SSE: {e}"
                        ))?;
                        let kind = event
                            .get("kind")
                            .and_then(|k| k.as_str())
                            .unwrap_or("");
                        match kind {
                            "status-update" => {
                                let final_ = event
                                    .get("final")
                                    .and_then(|f| f.as_bool())
                                    .unwrap_or(false);
                                let state = event["status"]["state"]
                                    .as_str()
                                    .unwrap_or("");
                                let msg_text = event["status"]["message"]
                                    ["parts"][0]["text"]
                                    .as_str()
                                    .unwrap_or("");
                                if !msg_text.is_empty() && msg_text != last_msg {
                                    last_msg = msg_text.to_owned();
                                    if let Some(ref bus) = self.event_bus {
                                        let marker = format!(
                                            "<rstool name=\"agent\">[{agent_id}] {msg_text}</rstool>"
                                        );
                                        let _ = bus.send(AgentEvent {
                                            session_id: ctx.session_key.clone(),
                                            agent_id: ctx.agent_id.clone(),
                                            delta: marker,
                                            done: false,
                                            files: vec![],
                                            images: vec![],
                                            tool_log: vec![],
                                            question: None,
                                            channel: None,
                                        });
                                    }
                                }
                                if final_ {
                                    if state == "TASK_STATE_FAILED"
                                        && !msg_text.is_empty()
                                    {
                                        last_error = Some(msg_text.to_owned());
                                    } else if state == "TASK_STATE_CANCELED" {
                                        return Err(anyhow!(
                                            "A2A remote `{agent_id}`: canceled"
                                        ));
                                    }
                                    break;
                                }
                            }
                            "artifact-update" => {
                                if let Some(parts) = event["artifact"]["parts"]
                                    .as_array()
                                {
                                    for part in parts {
                                        if part["type"] == "text"
                                            && let Some(t) = part["text"].as_str()
                                        {
                                            accumulated.push_str(t);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            if let Some(err) = last_error {
                return Err(anyhow!("A2A remote `{agent_id}`: {err}"));
            }
            if accumulated.is_empty() {
                return Err(anyhow!(
                    "A2A remote `{agent_id}`: no text artifact received"
                ));
            }
            return Ok(Value::String(accumulated));
        }

        Err(anyhow!(
            "A2A: agent `{agent_id}` not found locally or in external registry"
        ))
    }
}

/// Inject an `action` field into `args` if not already present.
pub(super) fn inject_action(mut args: Value, action: &str) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.entry("action").or_insert_with(|| json!(action));
    }
    args
}

/// Backward-compatible `/remember`/`memory_put` routing.
pub(super) fn inject_memory_put_compat(mut args: Value) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.entry("action").or_insert_with(|| json!("put"));
        obj.entry("kind").or_insert_with(|| json!("remember"));
    }
    args
}

/// Force `auth=true` on the wait-input args.
pub(super) fn inject_auth(mut args: Value) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.insert("auth".to_owned(), json!(true));
    }
    args
}

/// Inject a `channel` field into `args` if not already present.
pub(super) fn inject_channel(mut args: Value, channel: &str) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.entry("channel").or_insert_with(|| json!(channel));
    }
    args
}
