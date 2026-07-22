//! run_turn — drives a single conversation turn end-to-end.

use futures::StreamExt;

use super::*;

impl AgentRuntime {
    pub async fn run_turn(
        &mut self,
        session_key: &str,
        text: &str,
        channel: &str,
        peer_id: &str,
        chat_id: &str,
        account: Option<&str>,
        extra_tools: Vec<ToolDef>,
        images: Vec<crate::registry::ImageAttachment>,
        files: Vec<crate::registry::FileAttachment>,
        turn_ctx: crate::registry::TurnContext,
    ) -> Result<AgentReply> {
        // Refresh WASM plugins from the handle's shared slot so hot-reload
        // (rsclaw gateway reload --scope plugins) takes effect next turn.
        self.wasm_plugins = self.handle.wasm_plugins_snapshot();
        // Refresh providers from the handle's shared slot so hot-reload
        // (rsclaw gateway reload --scope providers) takes effect next turn.
        self.providers = self.handle.providers_snapshot();

        // Resolve @file references (e.g. @up_i_202604271325ab.png → full path
        // under workspace/uploads/, @dl_v_... → ~/Downloads/rsclaw/videos/).
        // Image references are auto-loaded as vision attachments.
        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.live.agents.read().await.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("workspace"));
        let resolved = rsclaw_channel::resolve_file_refs(text, &workspace);
        let text = resolved.text;

        // Channels that locally transcribe voice (WeChat platform STT,
        // Feishu speech recognition) tag the message with this prefix
        // so the agent knows the user spoke even though only text crosses
        // the on_message callback. Without this, voice_mode_sessions never
        // gets set on those channels and the reply goes back as text.
        const VOICE_INPUT_TAG: &str = "[__VOICE_INPUT__]";
        let text: String = if let Some(stripped) = text.strip_prefix(VOICE_INPUT_TAG) {
            self.voice_mode_sessions.insert(session_key.to_owned());
            debug!(
                session = session_key,
                "voice mode enabled (channel-side transcription tag)"
            );
            stripped.trim_start_matches('\n').to_owned()
        } else {
            text
        };
        let text = text.as_str();

        // Voice-mode toggle by natural language. Once a session is in voice
        // mode (either via /voice or because the user sent audio), the user
        // shouldn't have to remember the explicit /text command — phrases
        // like "用文字回复" or "no voice please" should switch back. Runs
        // before media detection so a typed instruction beats the
        // audio-attachment auto-enable that follows. Audio-only turns hit
        // this path on the recursive run_turn call after transcription.
        if let Some(want_voice) = parse_voice_mode_intent(text) {
            if want_voice {
                self.voice_mode_sessions.insert(session_key.to_owned());
                debug!(
                    session = session_key,
                    "voice mode enabled (natural-language intent)"
                );
            } else {
                self.voice_mode_sessions.remove(session_key);
                debug!(
                    session = session_key,
                    "voice mode disabled (natural-language intent)"
                );
            }
        }

        // Load @-referenced images as vision attachments. Used by desktop UI
        // (saves drop/paste files to ~/.rsclaw/workspace/uploads/i/ and inserts
        // `@up_i_<id>.<ext>`) and by HTTP/api clients that follow the same
        // upload convention. The bytes get downscaled before base64 — same
        // policy as the per-channel direct-push paths.
        let mut images = images;
        for img_path in &resolved.image_paths {
            if let Ok(bytes) = std::fs::read(img_path) {
                use base64::Engine;
                let ext = img_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("png");
                let orig_mime = match ext {
                    "jpg" | "jpeg" => "image/jpeg",
                    "webp" => "image/webp",
                    "gif" => "image/gif",
                    _ => "image/png",
                };
                let orig_len = bytes.len();
                let (final_bytes, final_mime) = rsclaw_util::downscale_image_for_vision(
                    &bytes,
                    orig_mime,
                    1 * 1024 * 1024,
                    1920,
                    85,
                )
                .unwrap_or_else(|e| {
                    tracing::warn!(path = %img_path.display(), error = %e, "@-ref: downscale failed");
                    (bytes, orig_mime.to_string())
                });
                let b64 = base64::engine::general_purpose::STANDARD.encode(&final_bytes);
                images.push(crate::registry::ImageAttachment {
                    data: format!("data:{final_mime};base64,{b64}"),
                    mime_type: final_mime,
                    source_path: Some(img_path.to_string_lossy().into_owned()),
                });
                info!(
                    path = %img_path.display(),
                    from = orig_len,
                    to = final_bytes.len(),
                    "loaded @-referenced image for vision"
                );
            }
        }

        // Resolve session key alias: if this key maps to a canonical (migrated)
        // key, use that so all messages stay under one session.
        let session_key = self.resolve_session_key(session_key).to_owned();
        let session_key = session_key.as_str();

        // cap_live sticky direct mode: if /cap <agent> was issued on this
        // IM session, route plain-text user messages straight to the cap
        // driver, skipping the main LLM. The slash-command registration
        // happens in preparse (`/cap`, `/cap-exit`); here we only consume
        // the binding. Slash commands that preparse didn't handle still
        // get routed to the driver — sticky means "everything to the
        // subagent."
        if let Some(manager) = self.cap_live_manager.as_ref() {
            if let Some((live_sid, kind)) = manager.resolve_sticky(session_key).await {
                // One-shot memory injection: on the FIRST user message
                // after a fresh `/cap <agent>` bind, prepend whatever
                // rsclaw's auto-recall has on this user (preferences,
                // prior facts, project context) so the cap subagent
                // starts the conversation with the same background the
                // main LLM would have had. Subsequent turns skip — the
                // driver process keeps its own history, so re-injecting
                // would just burn tokens. `/cap-resume` also skips
                // (resume_mode != None sets pending=false at spawn).
                let task = if manager.try_take_pending_memory_inject(&live_sid).await {
                    let mem_part = self
                        .build_auto_recall_bundle(&self.handle.id, channel, text)
                        .await
                        .filter(|b| !b.context.trim().is_empty());
                    let helper_part = build_cap_helper_cheatsheet(&self.skills);
                    if mem_part.is_some() || !helper_part.is_empty() {
                        tracing::info!(
                            target: "cap",
                            live_session_id = %live_sid,
                            mem_docs = mem_part.as_ref().map(|b| b.metadata.doc_ids.len()).unwrap_or(0),
                            helper_chars = helper_part.len(),
                            "sticky bypass: injecting first-turn background"
                        );
                        let mem_block = match &mem_part {
                            Some(b) => format!(
                                "## Long-term memory (from prior conversations)\n\n{}\n\n",
                                b.context.trim()
                            ),
                            None => String::new(),
                        };
                        format!(
                            "<background_from_main_agent_memory>\n\
                             You are a coding subagent that rsclaw (the main \
                             chat-side agent) has bridged into this user's \
                             IM session. The user can still read everything \
                             you say. Treat the items below as hints — they \
                             come from rsclaw's own memory and tooling — \
                             and verify before acting on them.\n\n\
                             {}{}\
                             </background_from_main_agent_memory>\n\n\
                             ---\n\n\
                             {}",
                            mem_block, helper_part, text
                        )
                    } else {
                        text.to_owned()
                    }
                } else {
                    text.to_owned()
                };
                tracing::info!(
                    target: "cap",
                    session = session_key,
                    live_session_id = %live_sid,
                    agent = kind.as_str(),
                    "sticky bypass: routing user message direct to cap driver"
                );
                let lang = self
                    .config
                    .raw
                    .gateway
                    .as_ref()
                    .and_then(|g| g.language.as_deref())
                    .map(rsclaw_i18n::resolve_lang)
                    .unwrap_or("en");

                // IM channels deliver the cap reply as ONE final message.
                // Most IM channels can't render token streaming, and the old
                // Phase-2b per-chunk bridge fragmented a reply mid-sentence
                // (even mid-URL) and repeated the channel footer on every
                // chunk. Desktop/WS still get live tokens by subscribing to
                // `event_bus` directly (keyed by `cap-live-<agent>-<sid>` —
                // filled synchronously by run_turn → bridge::dispatch), so
                // they are unaffected by delivering only the final reply to
                // the IM channel here.
                let dispatch_result = manager
                    .dispatch_sync(kind, Some(live_sid.clone()), task, workspace.clone(), None)
                    .await;

                match dispatch_result {
                    Ok(r) => {
                        // Deliver the full cap reply as a single message
                        // (or a "(no output)" marker when the agent said
                        // nothing).
                        let reply_text = r.output;
                        let is_empty = reply_text.trim().is_empty();
                        return Ok(AgentReply {
                            text: if is_empty {
                                rsclaw_i18n::t_fmt(
                                    "cap_no_output",
                                    lang,
                                    &[("agent", kind.display_name())],
                                )
                            } else {
                                reply_text
                            },
                            is_empty,
                            tool_calls: None,
                            images: vec![],
                            files: vec![],
                            pending_analysis: None,
                            needs_outer_done_emit: true,
                            outcome: crate::registry::ReplyOutcome::Ok,
                        });
                    }
                    Err(e) => {
                        // Driver died. Drop the binding so the user
                        // can /cap again, return the error message
                        // (it'll come through the normal reply path
                        // since chunker hasn't streamed an error).
                        let _ = manager.unbind_sticky(session_key).await;
                        let err = e.to_string();
                        let msg = rsclaw_i18n::t_fmt(
                            "cap_driver_error",
                            lang,
                            &[("agent", kind.as_str()), ("err", &err)],
                        );
                        return Ok(AgentReply {
                            text: msg,
                            is_empty: false,
                            tool_calls: None,
                            images: vec![],
                            files: vec![],
                            pending_analysis: None,
                            needs_outer_done_emit: true,
                            outcome: crate::registry::ReplyOutcome::Ok,
                        });
                    }
                }
            }
        }

        // Check clear_signal: if /clear was issued via bypass, clear sessions now.
        // Preserve a brief summary of each session so the agent retains key context.
        if self.handle.clear_signal.load(Ordering::SeqCst) {
            self.handle.clear_signal.store(false, Ordering::SeqCst);
            info!("clear_signal received, clearing all sessions");

            // Build summaries from existing sessions before clearing.
            let mut summary_msgs: Vec<(String, Message)> = Vec::new();
            for (key, messages) in &self.sessions {
                if let Some(msg) = build_clear_summary(messages) {
                    summary_msgs.push((key.clone(), msg));
                }
            }

            self.sessions.clear();
            self.compaction_state.clear();
            if let Ok(mut map) = self.handle.session_tokens.write() {
                map.clear();
            }
            // Also clear persisted sessions from redb (and their working
            // plans — session_key is stable per peer, so a stale todo would
            // leak into the next conversation after /clear).
            for key in self.store.db.list_sessions().unwrap_or_default() {
                let _ = self.store.db.delete_session(&key);
                if let Err(e) = self
                    .store
                    .db
                    .kv_delete(&crate::tools_misc::todo_kv_key(&key))
                {
                    tracing::warn!("todo kv cleanup failed for {key}: {e:#}");
                }
            }

            // Re-inject summaries so agent retains context, and persist to redb.
            for (key, msg) in summary_msgs {
                let val = serde_json::to_value(&msg).unwrap_or_default();
                if let Err(e) = self.store.db.append_message(&key, &val) {
                    tracing::warn!("failed to persist clear summary: {e:#}");
                }
                self.sessions.insert(key, vec![msg]);
            }
            // Refresh installed skills from disk (picks up skill_install/remove
            // since last load). Only invalidates the prompt cache if the set
            // actually changed — see reload_skills.
            self.reload_skills();
        }

        // /new — start a fresh conversation with new archive generation.
        if self.handle.new_session_signal.load(Ordering::SeqCst) {
            self.handle
                .new_session_signal
                .store(false, Ordering::SeqCst);
            info!("new_session_signal received, starting new generation");

            // Save session summary to memory before clearing — no summary
            // will be injected into the new session, so memory is the only
            // way the LLM can find prior context.
            let compaction_model = self
                .live
                .agents
                .read()
                .await
                .defaults
                .compaction
                .as_ref()
                .and_then(|c| c.model.clone())
                .or_else(|| {
                    self.handle
                        .config
                        .model
                        .as_ref()?
                        .primary_head()
                        .map(String::from)
                })
                .unwrap_or_else(|| "default".to_owned());
            self.save_session_summaries_to_memory(&compaction_model)
                .await;

            self.sessions.clear();
            self.compaction_state.clear();
            if let Ok(mut map) = self.handle.session_tokens.write() {
                map.clear();
            }
            for key in self.store.db.list_sessions().unwrap_or_default() {
                match self.store.db.new_generation(&key) {
                    Ok(g) => info!(session = %key, generation = g, "new generation started"),
                    Err(e) => tracing::warn!("failed to start new generation: {e:#}"),
                }
            }
            self.reload_skills();
        }

        // Reclaim idle browser session (kills Chrome process) to free memory.
        // Uses try_lock to avoid blocking if the browser is actively in use.
        if let Ok(mut guard) = self.browser.try_lock() {
            if let Some(ref session) = *guard {
                if session.is_idle_expired() {
                    info!("run_turn: browser idle timeout expired, closing Chrome to free memory");
                    *guard = None;
                }
            }
        }

        // Acquire concurrency permit (blocks if too many concurrent turns).
        let sem = Arc::clone(&self.handle.concurrency);
        let _permit = sem
            .acquire()
            .await
            .map_err(|_| anyhow!("agent concurrency semaphore closed"))?;

        // Update live status: turn started.
        if let Ok(mut status) = self.live_status.try_write() {
            status.state = "thinking".to_owned();
            let preview = text
                .char_indices()
                .nth(100)
                .map(|(i, _)| &text[..i])
                .unwrap_or(text);
            status.current_task = preview.to_owned();
            status.started_at = Some(std::time::Instant::now());
            status.session_key = session_key.to_owned();
            status.tool_history.clear();
            status.text_preview.clear();
        }

        let _agent_cfg = &self.handle.config;

        // Resolve language for user-facing channel messages.
        let i18n_lang = self
            .config
            .raw
            .gateway
            .as_ref()
            .and_then(|g| g.language.as_deref())
            .map(rsclaw_i18n::resolve_lang)
            .unwrap_or("en");

        // ---------------------------------------------------------------
        // Video menu: user replies 1-4 to a pending VIDEO upload.
        //   1. 提取音频转写  2. 分析画面  3. 转写+画面  4. 删除
        // ---------------------------------------------------------------
        let pending_response = text.trim();
        if matches!(pending_response, "1" | "2" | "3" | "4")
            && self.pending_files.get(session_key).is_some_and(|fs| {
                !fs.is_empty()
                    && fs
                        .iter()
                        .all(|f| matches!(f.stage, PendingStage::VideoMenu))
            })
        {
            let files = self.pending_files.remove(session_key).unwrap_or_default();
            return self
                .handle_video_menu_choice(
                    pending_response,
                    files,
                    session_key,
                    channel,
                    peer_id,
                    i18n_lang,
                )
                .await;
        }

        // ---------------------------------------------------------------
        // File action: user replies 1/2/3/4 to pending file prompt.
        //   1. 分析并保存  2. 分析后删除  3. 保存(已完成)  4. 删除
        // ---------------------------------------------------------------
        if (pending_response == "1" || pending_response == "2" || pending_response == "3")
            && let Some(files) = self.pending_files.remove(session_key)
            && !files.is_empty()
        {
            let workspace = self
                .handle
                .config
                .workspace
                .as_deref()
                .or(self.live.agents.read().await.defaults.workspace.as_deref())
                .map(expand_tilde)
                .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("workspace"));
            let uploads = workspace.join("uploads");

            match pending_response {
                "1" => {
                    // 分析并保存 / 保留
                    let upload_cfg = self
                        .config
                        .ext
                        .tools
                        .as_ref()
                        .and_then(|t| t.upload.as_ref());
                    let max_chars = upload_cfg
                        .and_then(|u| u.max_text_chars)
                        .unwrap_or(DEFAULT_MAX_TEXT_CHARS);
                    let mut analysis_text = String::new();
                    let mut binary_kept: Vec<(String, String)> = Vec::new();
                    for pf in &files {
                        if let PendingStage::TokenConfirm {
                            ref extracted_text, ..
                        } = pf.stage
                        {
                            let mut end = max_chars.min(extracted_text.len());
                            while end < extracted_text.len()
                                && !extracted_text.is_char_boundary(end)
                            {
                                end += 1;
                            }
                            let truncated = &extracted_text[..end];
                            analysis_text
                                .push_str(&format!("[File: {}]\n{}\n", pf.filename, truncated));
                        } else {
                            let subdir = rsclaw_channel::upload_subdir(&pf.mime_type, &pf.filename);
                            binary_kept.push((pf.filename.clone(), subdir.to_string()));
                        }
                        let _ = std::fs::remove_file(&pf.path);
                    }
                    // Binary-only: direct reply, no LLM.
                    if analysis_text.is_empty() {
                        let msg = binary_kept
                            .iter()
                            .map(|(name, subdir)| {
                                let suffix = rsclaw_i18n::t_fmt(
                                    "file_kept_in_uploads",
                                    &i18n_lang,
                                    &[("subdir", subdir.as_str())],
                                );
                                format!("- {name} {suffix}")
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        return Ok(AgentReply {
                            text: msg,
                            is_empty: false,
                            tool_calls: None,
                            images: vec![],
                            files: vec![],
                            pending_analysis: None,
                            // File-handling short-circuit bypasses agent_loop.
                            needs_outer_done_emit: true,
                            outcome: crate::registry::ReplyOutcome::Ok,
                        });
                    }
                    // Has extractable text: return "analyzing..." immediately,
                    // attach pending analysis for the per-user worker to process.
                    return Ok(AgentReply {
                        text: rsclaw_i18n::t("analyzing", i18n_lang),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: Some(crate::PendingAnalysis {
                            text: analysis_text,
                            session_key: session_key.to_owned(),
                            channel: channel.to_owned(),
                            peer_id: peer_id.to_owned(),
                        }),
                        // pending_analysis short-circuit bypasses agent_loop.
                        needs_outer_done_emit: true,
                        outcome: crate::registry::ReplyOutcome::Ok,
                    });
                }
                "2" => {
                    // 分析后删除
                    let upload_cfg = self
                        .config
                        .ext
                        .tools
                        .as_ref()
                        .and_then(|t| t.upload.as_ref());
                    let max_chars = upload_cfg
                        .and_then(|u| u.max_text_chars)
                        .unwrap_or(DEFAULT_MAX_TEXT_CHARS);
                    let mut analysis_text = String::new();
                    let mut binary_deleted = Vec::new();
                    for pf in &files {
                        if let PendingStage::TokenConfirm {
                            ref extracted_text, ..
                        } = pf.stage
                        {
                            let mut end = max_chars.min(extracted_text.len());
                            while end < extracted_text.len()
                                && !extracted_text.is_char_boundary(end)
                            {
                                end += 1;
                            }
                            let truncated = &extracted_text[..end];
                            analysis_text
                                .push_str(&format!("[File: {}]\n{}\n", pf.filename, truncated));
                        } else {
                            binary_deleted.push(pf.filename.clone());
                        }
                        let _ = std::fs::remove_file(&pf.path);
                        let _ = std::fs::remove_file(uploads.join(&pf.filename));
                    }
                    // Binary files: direct reply, no LLM needed.
                    if analysis_text.is_empty() {
                        let msg = if binary_deleted.is_empty() {
                            rsclaw_i18n::t("no_extractable_deleted", i18n_lang)
                        } else {
                            format!(
                                "{}\n{}",
                                binary_deleted
                                    .iter()
                                    .map(|f| format!("- {f}"))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                rsclaw_i18n::t("no_extractable_deleted", i18n_lang)
                            )
                        };
                        return Ok(AgentReply {
                            text: msg,
                            is_empty: false,
                            tool_calls: None,
                            images: vec![],
                            files: vec![],
                            pending_analysis: None,
                            // File-handling short-circuit bypasses agent_loop.
                            needs_outer_done_emit: true,
                            outcome: crate::registry::ReplyOutcome::Ok,
                        });
                    }
                    // Has extractable text: return "analyzing..." immediately,
                    // attach pending analysis for the per-user worker to process.
                    if !binary_deleted.is_empty() {
                        analysis_text.push_str(&format!(
                            "\n[Binary files deleted (no extractable text): {}]\n",
                            binary_deleted.join(", ")
                        ));
                    }
                    return Ok(AgentReply {
                        text: rsclaw_i18n::t("analyzing", i18n_lang),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: Some(crate::PendingAnalysis {
                            text: analysis_text,
                            session_key: session_key.to_owned(),
                            channel: channel.to_owned(),
                            peer_id: peer_id.to_owned(),
                        }),
                        // pending_analysis short-circuit bypasses agent_loop.
                        needs_outer_done_emit: true,
                        outcome: crate::registry::ReplyOutcome::Ok,
                    });
                }
                _ => {
                    // 直接删除
                    for pf in &files {
                        let _ = std::fs::remove_file(&pf.path);
                        let _ = std::fs::remove_file(uploads.join(&pf.filename));
                    }
                    return Ok(AgentReply {
                        text: rsclaw_i18n::t("files_deleted", i18n_lang),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                        // File-handling short-circuit bypasses agent_loop.
                        needs_outer_done_emit: true,
                        outcome: crate::registry::ReplyOutcome::Ok,
                    });
                }
            }
        }
        // Pre-parse: check for local commands before calling LLM
        let safety_on = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.safety)
            .unwrap_or(false);
        let preparse = crate::preparse::PreParseEngine::load_with_safety(safety_on);

        let is_default = self.handle.config.default.unwrap_or(false) || self.handle.id == "main";
        let allowed = self
            .handle
            .config
            .allowed_commands
            .as_deref()
            .unwrap_or(if is_default { "*" } else { "" });
        let cmd_permitted = |input: &str| -> bool {
            if allowed == "*" {
                return true;
            }
            let cmd = input.trim().split_whitespace().next().unwrap_or("");
            if READONLY_COMMANDS.iter().any(|c| *c == cmd) {
                return true;
            }
            if allowed.is_empty() {
                return false;
            }
            allowed.split('|').any(|a| a.trim() == cmd)
        };

        match preparse.try_parse(text) {
            crate::preparse::PreParseResult::PassThrough => {
                // Normal LLM flow continues below
            }
            crate::preparse::PreParseResult::DirectResponse(response) if cmd_permitted(text) => {
                // Handle special directives
                let reply_text = match response.as_str() {
                    "__HELP__" => {
                        let lang = self
                            .config
                            .raw
                            .gateway
                            .as_ref()
                            .and_then(|g| g.language.as_deref())
                            .map(rsclaw_i18n::resolve_lang)
                            .unwrap_or("en");
                        build_help_text_filtered(allowed, lang)
                    }
                    "__VERSION__" => format!(
                        "rsclaw {}",
                        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
                    ),
                    "__STATUS__" => self.handle.format_status(),
                    "__HEALTH__" => {
                        let model = self.resolve_model_name();
                        let (prov_name, _) =
                            rsclaw_provider::registry::ProviderRegistry::parse_model(&model);
                        let provider_ok = self.providers.get(prov_name).is_ok();
                        format!(
                            "Health check:\n  Provider ({}): {}\n  Store: ok\n  Agent: {}\n  Version: rsclaw {}",
                            model,
                            if provider_ok { "ok" } else { "unavailable" },
                            self.handle.id,
                            option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"),
                        )
                    }
                    "__UPTIME__" => format_duration(self.started_at.elapsed()),
                    "__MODELS__" => self.handle.format_models(),
                    s if s.starts_with("__MODEL_SET__:") => {
                        let model = s.strip_prefix("__MODEL_SET__:").unwrap_or("");
                        // Runtime-only model switch (doesn't persist to config)
                        // Update the agent handle's model config
                        format!(
                            "Model switched to: {model} (runtime only, use configure to persist)"
                        )
                    }
                    "__CLEAR__" => {
                        // Use LLM to generate a quality summary before clearing.
                        // The session may already be compacted, so input is small
                        // and the call is fast (~1-2s). No fact extraction needed
                        // because auto-compaction already did that.
                        let summary_text = if let Some(msgs) = self.sessions.get(session_key) {
                            if msgs.is_empty() {
                                None
                            } else {
                                let model = self.resolve_model_name();
                                // Single read guard for both defaults — the
                                // prior code held two consecutive
                                // `self.live.agents.read().await` calls a few
                                // tokens apart. Compaction-on-/clear runs on
                                // the user-input path so the contention is
                                // observable, not theoretical.
                                let (context_tokens, cfg) = {
                                    let agents = self.live.agents.read().await;
                                    (
                                        agents.defaults.context_tokens.unwrap_or(128_000) as usize,
                                        agents.defaults.compaction.clone().unwrap_or_default(),
                                    )
                                };
                                let default_transcript = (context_tokens * 7 / 10).max(16_000);
                                let max_transcript = cfg
                                    .max_transcript_tokens
                                    .map(|t| t as usize)
                                    .unwrap_or(default_transcript);
                                // Render transcript (reuse the same logic as compaction).
                                let transcript = Self::msgs_to_text_static(msgs, max_transcript);
                                let compaction_model = cfg.model.as_deref().unwrap_or(&model);
                                self.compact_single(compaction_model, &transcript, None)
                                    .await
                            }
                        } else {
                            None
                        };

                        self.sessions.remove(session_key);
                        self.handle.remove_session_tokens(session_key);
                        if let Err(e) = self.store.db.delete_session(session_key) {
                            warn!("failed to clear persisted session: {e:#}");
                        }
                        if let Some(summary) = summary_text {
                            let msg = Message {
                                role: rsclaw_provider::Role::User,
                                content: rsclaw_provider::MessageContent::Text(format!(
                                    "[Session summary before /clear]\n{summary}"
                                )),
                                rsclaw_hidden: None,
                            };
                            self.sessions.insert(session_key.to_owned(), vec![msg]);
                        }
                        rsclaw_i18n::t("session_cleared", rsclaw_i18n::default_lang()).to_owned()
                    }
                    "__COMPACT__" => {
                        // Manual compaction: force compress + save summary to memory.
                        let model = self.resolve_model_name();
                        self.compact_force(session_key, &model).await;
                        // Extract summary from the compacted session for memory storage.
                        // Look for the compaction-tagged message (role=User with COMPACTION
                        // prefix).
                        const COMPACTION_TAG: &str = "[CONTEXT COMPACTION";
                        if let Some(msgs) = self.sessions.get(session_key) {
                            let summary_text = msgs.iter().find_map(|m| {
                                let text = match &m.content {
                                    rsclaw_provider::MessageContent::Text(s) => s.clone(),
                                    rsclaw_provider::MessageContent::Parts(parts) => parts
                                        .iter()
                                        .filter_map(|p| {
                                            if let rsclaw_provider::ContentPart::Text { text } = p {
                                                Some(text.as_str())
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join(" "),
                                };
                                if text.starts_with(COMPACTION_TAG) {
                                    Some(text)
                                } else {
                                    None
                                }
                            });
                            if let Some(summary) = summary_text {
                                if let Some(ref mem) = self.memory {
                                    // UTF-8 safe truncation.
                                    let truncated: String = summary.chars().take(2000).collect();
                                    let mem_text =
                                        format!("Session compaction summary:\n{truncated}");
                                    let now = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs() as i64)
                                        .unwrap_or(0);
                                    let doc = crate::memory::MemoryDoc {
                                        id: uuid::Uuid::new_v4().to_string(),
                                        scope: "global".to_owned(),
                                        kind: "summary".to_owned(),
                                        text: mem_text.clone(),
                                        vector: vec![],
                                        created_at: now,
                                        accessed_at: now,
                                        access_count: 0,
                                        importance: 0.7,
                                        tier: Default::default(),
                                        abstract_text: None,
                                        overview_text: None,
                                        tags: vec![],
                                        pinned: false,
                                    };
                                    match mem.lock().await.add(doc).await {
                                        Ok(_) => info!(
                                            "compact: summary saved to memory ({} chars)",
                                            mem_text.len()
                                        ),
                                        Err(e) => warn!("compact: failed to save to memory: {e}"),
                                    }
                                }
                                rsclaw_i18n::t("compact_done", rsclaw_i18n::default_lang())
                                    .to_owned()
                            } else {
                                rsclaw_i18n::t(
                                    "compact_done_no_summary",
                                    rsclaw_i18n::default_lang(),
                                )
                                .to_owned()
                            }
                        } else {
                            rsclaw_i18n::t("compact_nothing", rsclaw_i18n::default_lang())
                                .to_owned()
                        }
                    }
                    "__ABORT__" => {
                        // Set abort flag for this session to interrupt running turn
                        let resolved_key = self.resolve_session_key(session_key);
                        let flags = self
                            .handle
                            .abort_flags
                            .write()
                            .expect("abort_flags lock poisoned");
                        if let Some(flag) = flags.get(resolved_key) {
                            flag.store(true, std::sync::atomic::Ordering::SeqCst);
                            "Abort signal sent. The running task will stop shortly.".to_owned()
                        } else {
                            "No active task found for this session.".to_owned()
                        }
                    }
                    "__TEXT_MODE__" => {
                        self.voice_mode_sessions.remove(session_key);
                        let zh = rsclaw_i18n::default_lang() == "zh";
                        if zh {
                            "已切换到文字回复模式。".to_owned()
                        } else {
                            "Switched to text reply mode.".to_owned()
                        }
                    }
                    "__VOICE_MODE__" => {
                        self.voice_mode_sessions.insert(session_key.to_owned());
                        let zh = rsclaw_i18n::default_lang() == "zh";
                        if zh {
                            "已切换到语音回复模式。".to_owned()
                        } else {
                            "Switched to voice reply mode.".to_owned()
                        }
                    }
                    s if s.starts_with("__HISTORY__:") => {
                        let n: usize = s
                            .strip_prefix("__HISTORY__:")
                            .unwrap_or("20")
                            .parse()
                            .unwrap_or(20);
                        if let Some(msgs) = self.sessions.get(session_key) {
                            let total_tokens: usize = msgs.iter().map(msg_tokens).sum();
                            let start = msgs.len().saturating_sub(n);
                            let mut lines = vec![format!(
                                "📊 Context: {} messages, ~{} tokens",
                                msgs.len(),
                                total_tokens
                            )];
                            for (i, msg) in msgs[start..].iter().enumerate() {
                                let role = match msg.role {
                                    rsclaw_provider::Role::User => "You",
                                    rsclaw_provider::Role::Assistant => "AI",
                                    rsclaw_provider::Role::System => "Sys",
                                    rsclaw_provider::Role::Tool => "Tool",
                                };
                                let text = match &msg.content {
                                    rsclaw_provider::MessageContent::Text(s) => s.clone(),
                                    rsclaw_provider::MessageContent::Parts(parts) => parts
                                        .iter()
                                        .filter_map(|p| {
                                            if let rsclaw_provider::ContentPart::Text { text } = p {
                                                Some(text.as_str())
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join(" "),
                                };
                                let preview: String = if text.chars().count() > 100 {
                                    text.chars().take(100).collect::<String>() + "..."
                                } else {
                                    text.clone()
                                };
                                lines.push(format!("{}. [{}] {}", start + i + 1, role, preview));
                            }
                            if lines.is_empty() {
                                "No messages in this session.".to_owned()
                            } else {
                                lines.join("\n")
                            }
                        } else {
                            "No messages in this session.".to_owned()
                        }
                    }
                    "__SESSIONS__" => {
                        if self.sessions.is_empty() {
                            "No active sessions.".to_owned()
                        } else {
                            let mut lines =
                                vec![format!("Active sessions: {}", self.sessions.len())];
                            for (key, msgs) in &self.sessions {
                                let short_key = if key.len() > 30 {
                                    let end = key
                                        .char_indices()
                                        .nth(30)
                                        .map(|(i, _)| i)
                                        .unwrap_or(key.len());
                                    &key[..end]
                                } else {
                                    key
                                };
                                lines.push(format!("  {} ({} messages)", short_key, msgs.len()));
                            }
                            lines.join("\n")
                        }
                    }
                    "__CRON_LIST__" => {
                        // Read the live job store at ~/.rsclaw/cron.json5 — this is the
                        // same source the cron runner and the `cron` tool use.  The
                        // previous implementation read self.config.ops.cron.jobs (static
                        // startup config) which is ALWAYS empty for tool-created jobs.
                        let cron_path = rsclaw_config::loader::base_dir().join("cron.json5");
                        let jobs = crate::tools_cron::read_cron_jobs(&cron_path).await;
                        crate::tools_cron::format_cron_jobs(&jobs)
                    }
                    "__GET_UPLOAD_SIZE__" => {
                        let max = self
                            .runtime_max_file_size
                            .or_else(|| {
                                self.config
                                    .ext
                                    .tools
                                    .as_ref()
                                    .and_then(|t| t.upload.as_ref())
                                    .and_then(|u| u.max_file_size)
                            })
                            .unwrap_or(DEFAULT_MAX_FILE_SIZE);
                        format!("Upload size limit: {} MB", max / 1_000_000)
                    }
                    s if s.starts_with("__SET_UPLOAD_SIZE__:") => {
                        let mb = s
                            .strip_prefix("__SET_UPLOAD_SIZE__:")
                            .unwrap_or("50")
                            .parse::<usize>()
                            .unwrap_or(50);
                        self.runtime_max_file_size = Some(mb * 1_000_000);
                        format!("Upload size limit set to {mb} MB (effective immediately)")
                    }
                    "__GET_UPLOAD_CHARS__" => {
                        let max_chars = self
                            .runtime_max_text_chars
                            .or_else(|| {
                                self.config
                                    .ext
                                    .tools
                                    .as_ref()
                                    .and_then(|t| t.upload.as_ref())
                                    .and_then(|u| u.max_text_chars)
                            })
                            .unwrap_or(DEFAULT_MAX_TEXT_CHARS);
                        let est_tokens = max_chars / 4;
                        format!("Max text per message: {max_chars} chars (~{est_tokens} tokens)")
                    }
                    s if s.starts_with("__SET_UPLOAD_CHARS__:") => {
                        let chars = s
                            .strip_prefix("__SET_UPLOAD_CHARS__:")
                            .unwrap_or("50000")
                            .parse::<usize>()
                            .unwrap_or(50000);
                        let est_tokens = chars / 4;
                        self.runtime_max_text_chars = Some(chars);
                        format!(
                            "Upload text limit set to {chars} chars (~{est_tokens} tokens, effective immediately)"
                        )
                    }
                    s if s.starts_with("__CONFIG_UPLOAD_SIZE__:") => {
                        let mb = s
                            .strip_prefix("__CONFIG_UPLOAD_SIZE__:")
                            .unwrap_or("50")
                            .parse::<usize>()
                            .unwrap_or(50);
                        let bytes = mb * 1_000_000;
                        self.runtime_max_file_size = Some(bytes);
                        match write_config_value(
                            "tools.upload.maxFileSize",
                            serde_json::json!(bytes),
                        ) {
                            Ok(()) => format!("Upload size limit set to {mb} MB (saved to config)"),
                            Err(e) => format!(
                                "Upload size limit set to {mb} MB (runtime only, config write failed: {e})"
                            ),
                        }
                    }
                    s if s.starts_with("__CONFIG_UPLOAD_CHARS__:") => {
                        let chars = s
                            .strip_prefix("__CONFIG_UPLOAD_CHARS__:")
                            .unwrap_or("50000")
                            .parse::<usize>()
                            .unwrap_or(50_000);
                        let est_tokens = chars / 4;
                        self.runtime_max_text_chars = Some(chars);
                        match write_config_value(
                            "tools.upload.maxTextChars",
                            serde_json::json!(chars),
                        ) {
                            Ok(()) => format!(
                                "Upload text limit set to {chars} chars (~{est_tokens} tokens, saved to config)"
                            ),
                            Err(e) => format!(
                                "Upload text limit set to {chars} chars (runtime only, config write failed: {e})"
                            ),
                        }
                    }
                    // --- Side-channel quick query (/btw) ---
                    s if s.starts_with("__SIDE_QUERY__:") => {
                        let question = s.strip_prefix("__SIDE_QUERY__:").unwrap_or("");
                        return self.handle_side_query(session_key, question).await;
                    }
                    s if s.starts_with("__") => {
                        text.to_owned() // fall through
                    }
                    "" => {
                        // Empty = suppress reply
                        return Ok(AgentReply {
                            text: String::new(),
                            is_empty: true,
                            tool_calls: None,
                            images: vec![],
                            files: vec![],
                            pending_analysis: None,
                            needs_outer_done_emit: true,
                            outcome: crate::registry::ReplyOutcome::Ok,
                        });
                    }
                    other => other.to_owned(),
                };
                if !reply_text.starts_with("__") {
                    return Ok(AgentReply {
                        text: reply_text,
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                        needs_outer_done_emit: true,
                        outcome: crate::registry::ReplyOutcome::Ok,
                    });
                }
                // Fall through to LLM for unhandled directives
            }
            crate::preparse::PreParseResult::ToolCall { tool, args } if cmd_permitted(text) => {
                // Group chat safety: block dangerous preparse commands (/run, /ls, /cat, etc.)
                let is_group = session_key.contains(":group:");
                if is_group
                    && matches!(
                        tool.as_str(),
                        "shell"
                            | "execute_command"
                            | "exec"
                            | "read_file"
                            | "read"
                            | "write_file"
                            | "write"
                    )
                {
                    return Ok(AgentReply {
                        text: "[Blocked] Shell/file commands are not allowed in group chats for security.".to_owned(),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                        needs_outer_done_emit: true,
                        outcome: crate::registry::ReplyOutcome::Ok,
                    });
                }
                info!(tool = %tool, "pre-parse: executing tool directly");
                // /remember command: inject kind=remember and action=put
                let args = if tool == "memory_put" {
                    let mut a = args;
                    a["kind"] = json!("remember");
                    a["action"] = json!("put");
                    a
                } else {
                    args
                };
                let result = self
                    .dispatch_tool(
                        &RunContext {
                            agent_id: self.handle.id.clone(),
                            session_key: session_key.to_owned(),
                            channel: channel.to_owned(),
                            peer_id: peer_id.to_owned(),
                            chat_id: String::new(),
                            account: None,
                            exec_pool: Arc::clone(&self.exec_pool),
                            loop_detector: crate::loop_detection::LoopDetector::default(),
                            has_images: false,
                            user_msg_with_images: None,
                            parse_error_count: 0,
                            recalled_memory_ids: std::collections::HashSet::new(),
                            auto_recall: None,
                            loop_warning_triggered: false,
                            loop_failure: None,
                            turn_metrics: crate::turn_metrics::TurnMetrics::new(),
                            user_text: String::new(),
                            full_trace: None,
                            turn_ctx: crate::registry::TurnContext::default(),
                        },
                        "",
                        &tool,
                        args.clone(),
                    )
                    .await;
                match result {
                    Ok(val) => {
                        let (reply_text, reply_images) =
                            if let Some(img) = val.get("image").and_then(|v| v.as_str()) {
                                ("".to_owned(), vec![img.to_owned()])
                            } else if val.is_string() {
                                (val.as_str().unwrap_or("").to_owned(), vec![])
                            } else {
                                (format_tool_result(&val), vec![])
                            };
                        return Ok(AgentReply {
                            text: reply_text.clone(),
                            is_empty: reply_text.is_empty() && reply_images.is_empty(),
                            tool_calls: None,
                            images: reply_images,
                            files: vec![],
                            pending_analysis: None,
                            needs_outer_done_emit: true,
                            outcome: crate::registry::ReplyOutcome::Ok,
                        });
                    }
                    Err(e) => {
                        return Ok(AgentReply {
                            text: format!("error: {e}"),
                            is_empty: false,
                            tool_calls: None,
                            images: vec![],
                            files: vec![],
                            pending_analysis: None,
                            needs_outer_done_emit: true,
                            outcome: crate::registry::ReplyOutcome::Ok,
                        });
                    }
                }
            }
            crate::preparse::PreParseResult::Blocked(reason) => {
                let safety_on = self
                    .config
                    .ext
                    .tools
                    .as_ref()
                    .and_then(|t| t.exec.as_ref())
                    .and_then(|e| e.safety)
                    .unwrap_or(false);
                if safety_on {
                    warn!(reason = %reason, "pre-parse: command blocked");
                    return Ok(AgentReply {
                        text: format!("[blocked] {reason}"),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                        needs_outer_done_emit: true,
                        outcome: crate::registry::ReplyOutcome::Ok,
                    });
                }
                // Safety off: fall through to execute anyway
            }
            crate::preparse::PreParseResult::NeedsConfirm { command, reason } => {
                let safety_on = self
                    .config
                    .ext
                    .tools
                    .as_ref()
                    .and_then(|t| t.exec.as_ref())
                    .and_then(|e| e.safety)
                    .unwrap_or(false);
                if safety_on {
                    return Ok(AgentReply {
                        text: format!(
                            "[confirm required] {reason}\nCommand: {command}\nReply 'yes' or 'y' to confirm."
                        ),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                        needs_outer_done_emit: true,
                        outcome: crate::registry::ReplyOutcome::Ok,
                    });
                }
                // Safety off: fall through to execute anyway
            }
            // Preparse matched a command but cmd_permitted denied it: block instead of falling
            // through to LLM
            crate::preparse::PreParseResult::DirectResponse(_)
            | crate::preparse::PreParseResult::ToolCall { .. } => {
                return Ok(AgentReply {
                    text: format!("Command not available on agent `{}`.", self.handle.id),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    needs_outer_done_emit: true,
                    outcome: crate::registry::ReplyOutcome::Ok,
                });
            }
        }

        let agent_cfg = &self.handle.config;

        // Direct reply (e.g. file too large) -- return without LLM
        if text.starts_with("__DIRECT_REPLY__") {
            let reply = text.strip_prefix("__DIRECT_REPLY__").unwrap_or(text);
            return Ok(AgentReply {
                text: reply.to_owned(),
                is_empty: false,
                tool_calls: None,
                images: vec![],
                files: vec![],
                pending_analysis: None,
                // __DIRECT_REPLY__ bypasses agent_loop.
                needs_outer_done_emit: true,
                outcome: crate::registry::ReplyOutcome::Ok,
            });
        }

        // ---------------------------------------------------------------
        // File attachment: auto-transcribe audio only.
        // Video and other files go through the normal file-save path
        // (user chooses analyze/save via the PendingFile prompt).
        // Doubao's vision API cannot decode inline base64 video, so we no
        // longer wrap videos as ImageAttachments — they would be rejected
        // with "Invalid base64 image_url".
        // ---------------------------------------------------------------
        let mut images = images;
        let (media_files, regular_files): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| {
            rsclaw_channel::is_audio_attachment(&f.mime_type, &f.filename)
                && !rsclaw_channel::is_video_attachment(&f.mime_type, &f.filename)
        });
        let mut files = regular_files;

        // Convert NEW images to FileAttachments so they go through the
        // unified pending-file flow (save → menu → user choice).
        // Skip this for:
        //   - @-referenced images (already on disk, going to vision)
        //   - Inline image+text turns (desktop/ws/a2a/api callers that pack a question
        //     alongside the image; the user clearly wants this image analysed THIS turn
        //     — the save-and-ask menu is friction left over from old single-payload
        //     channels like feishu/wechat where image-message and text-message arrive
        //     separately)
        let is_ref_image = !resolved.image_paths.is_empty();
        let is_inline_image = !text.trim().is_empty() && !images.is_empty();
        if !images.is_empty() && !is_ref_image && !is_inline_image {
            for img in &images {
                use base64::Engine;
                let b64 = img
                    .data
                    .strip_prefix("data:image/png;base64,")
                    .or_else(|| img.data.strip_prefix("data:image/jpeg;base64,"))
                    .or_else(|| img.data.strip_prefix("data:image/webp;base64,"))
                    .or_else(|| img.data.strip_prefix("data:image/gif;base64,"))
                    .unwrap_or(&img.data);
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                    let ext = if img.mime_type.contains("jpeg") || img.mime_type.contains("jpg") {
                        "jpg"
                    } else if img.mime_type.contains("webp") {
                        "webp"
                    } else if img.mime_type.contains("gif") {
                        "gif"
                    } else {
                        "png"
                    };
                    let mime = if img.mime_type.is_empty() {
                        format!("image/{ext}")
                    } else {
                        img.mime_type.clone()
                    };
                    files.push(crate::registry::FileAttachment {
                        filename: format!("image.{ext}"),
                        data: bytes,
                        mime_type: mime,
                    });
                }
            }
            images = vec![];
        }

        if !media_files.is_empty() {
            // Auto-enable voice mode when user sends audio (not video).
            let has_audio = media_files.iter().any(|f| {
                rsclaw_channel::is_audio_attachment(&f.mime_type, &f.filename)
                    && !rsclaw_channel::is_video_attachment(&f.mime_type, &f.filename)
            });
            if has_audio {
                self.voice_mode_sessions.insert(session_key.to_owned());
                debug!(
                    session = session_key,
                    "voice mode enabled (audio attachment detected)"
                );
            }
            let mut transcriptions = Vec::new();
            for mf in &media_files {
                if let Some(t) =
                    rsclaw_channel::extract_audio_text(&mf.data, &mf.filename.to_lowercase()).await
                {
                    info!(chars = t.len(), file = %mf.filename, "media transcribed from file attachment");
                    transcriptions.push(format!("[{}]\n{}", mf.filename, t));
                } else {
                    transcriptions.push(format!("[{} (transcription failed)]", mf.filename));
                }
            }
            if !transcriptions.is_empty() && files.is_empty() {
                let combined = transcriptions.join("\n\n");
                let full_text = if text.is_empty() {
                    combined
                } else {
                    format!("{text}\n\n{combined}")
                };
                return Box::pin(self.run_turn(
                    session_key,
                    &full_text,
                    channel,
                    peer_id,
                    chat_id,
                    account,
                    extra_tools,
                    images,
                    vec![],
                    turn_ctx,
                ))
                .await;
            } else if !transcriptions.is_empty() {
                let combined = transcriptions.join("\n\n");
                let full_text = if text.is_empty() {
                    combined
                } else {
                    format!("{text}\n\n{combined}")
                };
                return Box::pin(self.run_turn(
                    session_key,
                    &full_text,
                    channel,
                    peer_id,
                    chat_id,
                    account,
                    extra_tools,
                    images,
                    files,
                    turn_ctx,
                ))
                .await;
            }
        }

        // ---------------------------------------------------------------
        // File attachment: auto-save + show 3-option menu
        // ---------------------------------------------------------------
        // Images arriving as FILE attachments (feishu large-image → file, a
        // dropped image file, etc.) get analyzed INLINE this turn via the
        // vision path — NOT parked in the file-confirm menu. That menu turned
        // image analysis into a two-step "已收到图片@up_… → 分析中 → 结果" flow;
        // users expect one-shot recognition. Pull images out of `files` into
        // `images` (still written to uploads/ so `@up_<id>` references keep
        // working); only non-image files fall through to the confirm menu.
        let (image_files, other_files): (Vec<_>, Vec<_>) = files
            .into_iter()
            .partition(|f| f.mime_type.starts_with("image/"));
        let mut images = images;
        if !image_files.is_empty() {
            use base64::Engine as _;
            let ws = agent_cfg
                .workspace
                .as_deref()
                .or(self.live.agents.read().await.defaults.workspace.as_deref())
                .map(expand_tilde)
                .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("workspace"));
            let uploads = ws.join("uploads");
            for f in image_files {
                let subdir = rsclaw_channel::upload_subdir(&f.mime_type, &f.filename);
                let std_name = rsclaw_channel::upload_filename(&f.mime_type, &f.filename);
                let dir = uploads.join(subdir);
                let _ = std::fs::create_dir_all(&dir);
                let saved = dir.join(&std_name);
                let _ = std::fs::write(&saved, &f.data);
                let b64 = base64::engine::general_purpose::STANDARD.encode(&f.data);
                images.push(crate::registry::ImageAttachment {
                    data: format!("data:{};base64,{}", f.mime_type, b64),
                    mime_type: f.mime_type,
                    source_path: Some(saved.to_string_lossy().into_owned()),
                });
            }
        }
        let files = other_files;

        if !files.is_empty() {
            let ws = agent_cfg
                .workspace
                .as_deref()
                .or(self.live.agents.read().await.defaults.workspace.as_deref())
                .map(expand_tilde)
                .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("workspace"));
            let uploads = ws.join("uploads");
            let _ = std::fs::create_dir_all(&uploads);

            // Check file size limits
            let upload_cfg = self
                .config
                .ext
                .tools
                .as_ref()
                .and_then(|t| t.upload.as_ref());
            let max_file_size = self
                .runtime_max_file_size
                .or_else(|| upload_cfg.and_then(|u| u.max_file_size))
                .unwrap_or(DEFAULT_MAX_FILE_SIZE);
            let mut rejected = Vec::new();
            let mut accepted = Vec::new();
            for f in files {
                if f.data.len() > max_file_size {
                    rejected.push(format!(
                        "- {} ({:.1} MB)",
                        f.filename,
                        f.data.len() as f64 / 1e6
                    ));
                } else {
                    accepted.push(f);
                }
            }
            if !rejected.is_empty() && accepted.is_empty() {
                let limit_str = format!("{:.0}", max_file_size as f64 / 1e6);
                let msg =
                    rsclaw_i18n::t_fmt("file_size_exceeded", i18n_lang, &[("limit", &limit_str)]);
                let adjust = rsclaw_i18n::t("file_size_adjust", i18n_lang);
                return Ok(AgentReply {
                    text: format!("{msg}\n{}\n\n{adjust}", rejected.join("\n")),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    // File-size-exceeded short-circuit bypasses agent_loop.
                    needs_outer_done_emit: true,
                    outcome: crate::registry::ReplyOutcome::Ok,
                });
            }
            let files = accepted;

            // Check disk space before saving
            let total_size: usize = files.iter().map(|f| f.data.len()).sum();
            let available = fs2::available_space(&uploads).unwrap_or(u64::MAX);
            // Require at least 100MB headroom beyond file size
            if (total_size as u64) + 100_000_000 > available {
                let avail_mb = available / 1_000_000;
                let need_mb = total_size / 1_000_000;
                return Ok(AgentReply {
                    text: rsclaw_i18n::t_fmt(
                        "disk_space_low",
                        i18n_lang,
                        &[
                            ("need", &need_mb.to_string()),
                            ("avail", &avail_mb.to_string()),
                        ],
                    ),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    // Disk-low short-circuit bypasses agent_loop.
                    needs_outer_done_emit: true,
                    outcome: crate::registry::ReplyOutcome::Ok,
                });
            }

            let mut file_info = Vec::new();
            for file in files {
                // Route to type-specific subdirectory with standardized filename.
                let subdir = rsclaw_channel::upload_subdir(&file.mime_type, &file.filename);
                let std_name = rsclaw_channel::upload_filename(&file.mime_type, &file.filename);
                let target_dir = uploads.join(subdir);
                let _ = std::fs::create_dir_all(&target_dir);
                let dest = target_dir.join(&std_name);
                let size = file.data.len();
                let _ = std::fs::write(&dest, &file.data);

                // Images: mark as vision-analyzable. Video/audio: binary.
                // Others: try text extraction.
                let is_image = file.mime_type.starts_with("image/");
                let extracted = if is_image {
                    // Placeholder — actual analysis via vision when user chooses "1".
                    Some(format!("[image:vision:@{std_name}]"))
                } else if rsclaw_channel::is_video_attachment(&file.mime_type, &file.filename)
                    || rsclaw_channel::is_audio_attachment(&file.mime_type, &file.filename)
                {
                    None
                } else {
                    rsclaw_channel::extract_file_text(&file.filename, &file.data).await
                };
                let has_text = extracted.is_some();
                let est_tokens = extracted.as_ref().map(|t| estimate_tokens(t)).unwrap_or(0);

                file_info.push((std_name.clone(), size, has_text, est_tokens));

                // Store pending for later analysis. Videos skip the temp
                // duplicate (they're big and already saved to uploads/) and
                // carry the real path for ffmpeg + the delete option.
                let is_video = rsclaw_channel::is_video_attachment(&file.mime_type, &file.filename);
                let (path, stage) = if is_video {
                    (dest.clone(), PendingStage::VideoMenu)
                } else {
                    let path =
                        std::env::temp_dir().join(format!("rsclaw_pending_{}.bin", Uuid::new_v4()));
                    let _ = std::fs::write(&path, &file.data);
                    let stage = if let Some(ext_text) = extracted {
                        PendingStage::TokenConfirm {
                            extracted_text: ext_text,
                            estimated_tokens: est_tokens,
                        }
                    } else {
                        PendingStage::SizeConfirm
                    };
                    (path, stage)
                };
                self.pending_files
                    .entry(session_key.to_owned())
                    .or_default()
                    .push(PendingFile {
                        filename: std_name,
                        path,
                        size,
                        mime_type: file.mime_type,
                        images: vec![],
                        stage,
                    });
            }

            let file_list: String = file_info
                .iter()
                .map(|(name, size, has_text, tokens)| {
                    let size_str = if *size > 1_000_000 {
                        format!("{:.1} MB", *size as f64 / 1_000_000.0)
                    } else {
                        format!("{:.1} KB", *size as f64 / 1_000.0)
                    };
                    let analysis = if *has_text {
                        rsclaw_i18n::t_fmt(
                            "file_analyzable",
                            i18n_lang,
                            &[("tokens", &tokens.to_string())],
                        )
                    } else {
                        rsclaw_i18n::t("file_binary", i18n_lang)
                    };
                    format!("- {name} ({size_str}, {analysis})")
                })
                .collect::<Vec<_>>()
                .join("\n");

            let saved_msg = if i18n_lang == "zh" {
                format!("已保存 {} 个文件:", file_info.len())
            } else {
                format!("{} file(s) saved:", file_info.len())
            };
            let any_analyzable = file_info.iter().any(|(_, _, has_text, _)| *has_text);
            let all_videos = self.pending_files.get(session_key).is_some_and(|fs| {
                !fs.is_empty()
                    && fs
                        .iter()
                        .all(|f| matches!(f.stage, PendingStage::VideoMenu))
            });
            let menu_msg = if all_videos {
                rsclaw_i18n::t("video_menu", i18n_lang)
            } else if any_analyzable {
                rsclaw_i18n::t("file_menu", i18n_lang)
            } else {
                // Binary only -- simplified menu.
                if i18n_lang == "zh" {
                    "1. 保留\n2. 删除".to_owned()
                } else {
                    "1. Keep\n2. Delete".to_owned()
                }
            };
            let ref_hint = file_info
                .iter()
                .map(|(name, _, _, _)| format!("@{name}"))
                .collect::<Vec<_>>()
                .join(" ");
            let ref_msg = if i18n_lang == "zh" {
                format!("引用: {ref_hint}")
            } else {
                format!("Reference: {ref_hint}")
            };
            let reply = format!("{saved_msg}\n{file_list}\n{ref_msg}\n\n{menu_msg}");
            return Ok(AgentReply {
                text: reply,
                is_empty: false,
                tool_calls: None,
                images: vec![],
                files: vec![],
                pending_analysis: None,
                // File-saved short-circuit bypasses agent_loop.
                needs_outer_done_emit: true,
                outcome: crate::registry::ReplyOutcome::Ok,
            });
        }

        // (Old two-layer image/text gate removed -- files handled above)

        // Workspace path — expand leading `~/` so dynamically spawned agents work.
        let workspace = agent_cfg
            .workspace
            .as_deref()
            .or(self.live.agents.read().await.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("workspace"));

        // Load workspace context (cached -- only re-reads files whose mtime changed).
        let mut ws_ctx = {
            let cache = self
                .workspace_cache
                .get_or_insert_with(|| crate::workspace::WorkspaceCache::new(&workspace));
            cache.load(
                SessionType::Normal,
                true,
                DEFAULT_MAX_CHARS_PER_FILE,
                DEFAULT_TOTAL_MAX_CHARS,
            )
        };

        // Fingerprint the persona inputs so a freshly-created/edited workspace
        // file (or a changed inline `system`) rebuilds the cached prompt without
        // a gateway restart. ws_ctx is already fresh (cache re-reads by mtime);
        // only the assembled prompt was stuck.
        let ws_fingerprint = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            ws_ctx.agents_md.hash(&mut h);
            ws_ctx.soul_md.hash(&mut h);
            ws_ctx.user_md.hash(&mut h);
            ws_ctx.identity_md.hash(&mut h);
            ws_ctx.tools_md.hash(&mut h);
            self.handle.config.system.hash(&mut h);
            h.finish()
        };
        if self.cached_prompt_fingerprint != Some(ws_fingerprint) {
            self.cached_system_prompt = None;
            self.cached_prompt_fingerprint = Some(ws_fingerprint);
        }

        // Precedence: an explicit inline `system` OVERRIDES the workspace
        // SOUL.md persona (one authoritative persona, no double-identity). Drop
        // soul_md before the prompt renders it; other workspace files (AGENTS.md
        // project rules, USER.md, MEMORY.md, TOOLS.md) still load — they're not
        // the persona and don't conflict with the inline system.
        let has_inline_system = self
            .handle
            .config
            .system
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        if has_inline_system {
            ws_ctx.soul_md = None;
        }

        // Build system prompt — cached for entire gateway lifetime.
        // Only rebuilt on gateway restart.
        //
        // Heartbeat / system auto-tick sessions get a minimal prompt
        // since they only have the `memory` tool and don't need workspace
        // files, skills, or tool guidelines. Saves ~3k tokens per tick.
        // Cron is excluded: cron-fired agentTurn carries real user
        // instructions and needs the full prompt + tool set.
        let is_internal = is_minimal_context_session(session_key);
        let system_prompt = if is_internal {
            if self.cached_minimal_prompt.is_none() {
                self.cached_minimal_prompt = Some(build_minimal_system_prompt());
            }
            self.cached_minimal_prompt.clone().expect("just set")
        } else {
            if self.cached_system_prompt.is_none() {
                // Resolve toolset from per-agent model config (falling back to
                // gateway defaults), so build_user_system can branch on
                // `toolset=="code"` and append the coding profile block.
                let toolset_owned: Option<String> = self
                    .handle
                    .config
                    .model
                    .as_ref()
                    .or(self.config.agents.defaults.model.as_ref())
                    .and_then(|m| m.toolset.as_deref())
                    .map(|s| s.to_owned());
                let mut prompt = build_system_prompt(
                    &ws_ctx,
                    &self.skills,
                    &self.wasm_plugins,
                    self.plugins.as_deref(),
                    &self.config.raw,
                    toolset_owned.as_deref(),
                    self.cap_manager.is_some(),
                );
                // Per-agent inline persona from `agents.list[].system`. This is
                // agent-specific → appended to the user_system tail (NOT the
                // shared cross-agent prefix). The field had NO consumer after
                // the crate-split, so agents that set an inline `system`
                // (instead of a workspace SOUL.md) silently got only the
                // generic default persona ("你是谁" → "螃蟹助手").
                if let Some(sys) = self
                    .handle
                    .config
                    .system
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    prompt.push_str("\n\n");
                    prompt.push_str(sys);
                }
                // DEBUG: dump full system prompt to file for inspection
                if std::env::var("RSCLAW_DUMP_PROMPT").is_ok() {
                    let dump_path =
                        rsclaw_config::loader::base_dir().join("debug_system_prompt.txt");
                    if let Err(e) = std::fs::write(&dump_path, &prompt) {
                        tracing::warn!("failed to dump system prompt: {e}");
                    }
                    tracing::info!(path = %dump_path.display(), len = prompt.len(), "dumped system prompt");
                }
                self.cached_system_prompt = Some(prompt);
            }
            self.cached_system_prompt.clone().expect("just set")
        };

        // Plugin hook: before_prompt_build (AGENTS.md §20).
        self.fire_hook(
            "before_prompt_build",
            json!({
                "agent_id": self.handle.id,
                "session_key": session_key,
                "channel": channel,
            }),
        )
        .await;

        // Resolve model.
        //
        // Internal auto-tick sessions (heartbeat / system) only hold the
        // `memory` tool and run on a cadence — the primary slot is wasted
        // on memory upkeep / HEARTBEAT_OK replies. Route them through the
        // flash model when configured. `resolve_flash_model_name` already
        // falls back to the primary if no flash model is set, so this is
        // safe with any provider configuration.
        //
        // Cron is intentionally excluded from this fast path: cron-fired
        // agentTurns carry real user instructions and need primary-tier
        // reasoning.
        let model = if is_internal {
            self.resolve_flash_model_name()
        } else {
            agent_cfg
                .model
                .as_ref()
                .and_then(|m| m.primary_head())
                .or_else(|| {
                    self.config
                        .agents
                        .defaults
                        .model
                        .as_ref()
                        .and_then(|m| m.primary_head())
                })
                .unwrap_or("rsclaw/rsclaw-agent-v1")
                .to_owned()
        };
        // Resolve the rest of the primary chain (everything after the head)
        // for failover. Empty when primary is a single string or when this
        // is an internal flash call — both cases preserve the legacy
        // single-model + global-fallback behaviour. The chain falls back
        // through the standard layered config: per-agent > defaults.
        let primary_chain_tail: Vec<String> = if is_internal {
            Vec::new()
        } else {
            let chain = agent_cfg
                .model
                .as_ref()
                .map(|m| m.primary_chain())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| {
                    self.config
                        .agents
                        .defaults
                        .model
                        .as_ref()
                        .map(|m| m.primary_chain())
                        .unwrap_or_default()
                });
            chain.into_iter().skip(1).map(String::from).collect()
        };
        // Owned copy: the borrow from resolve_model must not outlive the
        // &mut self calls below (session load, todo kv reads).
        let model_provider = self.providers.resolve_model(&model).0.to_owned();

        // Loop A (organic evolution): collect recalled memory IDs for feedback.
        // Auto-recall is turn-local committed recall. It is enabled only for
        // native rsclaw sessions; external providers do not understand hidden
        // replay state and must not persist rsclaw_hidden they never consumed.
        // Recall is built for every provider; only DELIVERY differs —
        // rsclaw rides the hidden side channel, everything else gets a
        // turn-local text injection at request-build time (see the main
        // loop). memory.recallExternalProviders=false restores the old
        // rsclaw-only gate.
        let recall_external = self
            .config
            .agents
            .defaults
            .memory
            .as_ref()
            .and_then(|m| m.recall_external_providers)
            .unwrap_or(true);
        let auto_recall_bundle = if model_provider == "rsclaw" || recall_external {
            self.build_auto_recall_bundle(&self.handle.id, channel, text)
                .await
        } else {
            None
        };
        // Keep the session's working plan (todo tool) visible every turn.
        // It rides the turn-local recall channel, so it never dirties the
        // KV prefix and survives the original tool result being sketched
        // out of the transcript.
        let auto_recall_bundle = if model_provider == "rsclaw" || recall_external {
            match (auto_recall_bundle, self.load_todo_rendered(session_key)) {
                (bundle, None) => bundle,
                (Some(mut bundle), Some(plan)) => {
                    bundle.context.push_str("\n\n[Current plan]\n");
                    bundle.context.push_str(&plan);
                    bundle.metadata.hash = {
                        use sha2::{Digest, Sha256};
                        format!("sha256:{:x}", Sha256::digest(bundle.context.as_bytes()))
                    };
                    Some(bundle)
                }
                (None, Some(plan)) => {
                    let context = format!("[Current plan]\n{plan}");
                    let hash = {
                        use sha2::{Digest, Sha256};
                        format!("sha256:{:x}", Sha256::digest(context.as_bytes()))
                    };
                    Some(RecallBundle {
                        context,
                        metadata: rsclaw_provider::RecallMetadata {
                            source: "todo".to_owned(),
                            hash,
                            ..Default::default()
                        },
                    })
                }
            }
        } else {
            auto_recall_bundle
        };
        let auto_recalled_ids = auto_recall_bundle
            .as_ref()
            .map(|b| b.metadata.doc_ids.iter().cloned().collect())
            .unwrap_or_else(std::collections::HashSet::<String>::new);

        // Build tool list from skills and registered agents (local + remote).
        // Tool selection: toolsEnabled -> toolset level -> tools whitelist
        let model_cfg = self.handle.config.model.as_ref().or(self
            .config
            .agents
            .defaults
            .model
            .as_ref());
        let tools_enabled = model_cfg.and_then(|m| m.tools_enabled).unwrap_or(true);

        // `summarize:<original_session>` is the cron summarizer's session
        // prefix. The summarize turn must produce a plain-text summary, not
        // a tool call (memory.put / write_file / etc.) — otherwise the cron
        // delivers a tool acknowledgement instead of the actual summary.
        // Force the tool list empty so the LLM has no choice but to
        // respond with text.
        let is_summarize_turn = session_key.starts_with("summarize:");

        // Cold ToolDefs pulled out by the deferral below; handed to
        // agent_loop, which splices them back into the live tool list when
        // the model calls `request_tool`.
        // (enable_key, ToolDef): key is the builtin tool name for cold
        // tools, `pg:<plugin>:<group>` for plugin tool groups.
        let mut deferred_tool_defs: Vec<(String, rsclaw_provider::ToolDef)> = Vec::new();
        // Stub lines offered by request_tool: (enable name, display label).
        let mut stub_entries: Vec<(String, String)> = Vec::new();
        let tools = if !tools_enabled || is_summarize_turn {
            vec![]
        } else {
            // Build full tool list first
            let mut all = build_tool_list(
                &self.skills,
                self.agents.as_deref(),
                &self.handle.id,
                &self.config.agents.a2a,
            );
            // Cold pool snapshot BEFORE toolset filtering: request_tool is an
            // on-demand tier over the FULL builtin set, not just the agent's
            // toolset — a `standard` agent asked for a .docx must be able to
            // request create_docx even though the preset excludes it (its
            // only alternative is faking the binary with write_file, which
            // tool_write now rejects).
            let cold_pool: Vec<rsclaw_provider::ToolDef> = all
                .iter()
                .filter(|t| crate::tools_builder::COLD_TOOLS.contains(&t.name.as_str()))
                .cloned()
                .collect();
            all.extend(extra_tools.iter().cloned());
            // Plugin tools: `plugin_search`/`plugin_describe`/`plugin_invoke`
            // builtin meta-tools remain the long-tail discovery path. On top
            // of that, headline-marked tools (plus `model.plugin_tools` pins
            // and session `/plugin pin` overrides, minus
            // `plugin_tools_unpin` / `/plugin unpin`) are auto-promoted into
            // `dynamic_prefix.user_tools` as real namespaced ToolDefs
            // (`<plugin>__<tool>`). This sits in the per-session user_tools
            // cache segment (v1.9 rsclaw protocol), so per-client variance
            // doesn't dirty the base prefix.
            if let Some(ref mcp) = self.mcp {
                all.extend(mcp.all_tool_defs().await);
            }
            if !self.has_stock_tool_provider() {
                all.retain(|t| !is_stock_tool_name(&t.name));
            }
            // user_tools_cap default — 30 fits ~10 headlines × 3 plugins
            // without overflowing a 64k-context small model.
            let cap = model_cfg
                .and_then(|m| m.user_tools_cap)
                .unwrap_or(DEFAULT_USER_TOOLS_CAP);
            // v2: token budget replaces the count cap when configured.
            let budget = model_cfg.and_then(|m| m.user_tools_budget);
            let empty_vec: Vec<String> = Vec::new();
            let config_pin = model_cfg
                .and_then(|m| m.plugin_tools.as_ref())
                .unwrap_or(&empty_vec);
            let config_unpin = model_cfg
                .and_then(|m| m.plugin_tools_unpin.as_ref())
                .unwrap_or(&empty_vec);
            let session_overrides = self
                .handle
                .plugin_overrides
                .read()
                .ok()
                .and_then(|g| g.get(session_key).cloned())
                .unwrap_or_default();
            // v2 group pins: config `model.pluginGroups` ∪ session enables
            // recorded by request_tool ("pg:<plugin>:<group>" markers in
            // the cold_enabled map, namespace stripped here).
            let mut group_pins: std::collections::BTreeSet<String> = model_cfg
                .and_then(|m| m.plugin_groups.as_ref())
                .map(|v| v.iter().cloned().collect())
                .unwrap_or_default();
            if let Ok(g) = self.handle.cold_enabled.read()
                && let Some(set) = g.get(session_key)
            {
                for k in set {
                    if let Some(rest) = k.strip_prefix("pg:") {
                        group_pins.insert(rest.to_owned());
                    }
                }
            }
            let selections = Self::select_user_tools_pure(
                &self.wasm_plugins,
                self.plugins.as_deref(),
                &session_overrides,
                config_pin,
                config_unpin,
                &group_pins,
                cap,
                budget,
            );
            // v2: grouped tools that didn't make the live set ride behind
            // the request_tool stub — for EVERY provider. Unlike builtin
            // cold tools (rsclaw keeps those in its cached prefix), plugin
            // tools live in the per-session user_tools segment, so the
            // budget/stub math applies to rsclaw and external alike.
            let (group_defs, group_lines) = Self::collect_deferred_group_tools(
                &self.wasm_plugins,
                self.plugins.as_deref(),
                &selections,
            );
            for (key, label) in group_lines {
                stub_entries.push((key, label));
            }
            deferred_tool_defs.extend(group_defs);
            for sel in selections {
                let wire_name = sel.wire_name();
                all.push(rsclaw_provider::ToolDef {
                    name: wire_name,
                    description: sel.description.clone(),
                    parameters: sel.input_schema.clone(),
                });
            }

            // Apply toolset level + custom tools list
            // Default agent uses "full", others use "standard". Fall back to
            // `id == "main"` so configs that omit the explicit `default: true`
            // flag (newer setup template) still resolve main as the default —
            // matches the convention used at runtime.rs:1763 and
            // RuntimeConfig::default_agent.
            let is_default =
                self.handle.config.default.unwrap_or(false) || self.handle.id == "main";
            let default_toolset = if is_default { "full" } else { "standard" };
            let toolset = model_cfg
                .and_then(|m| m.toolset.as_deref())
                .unwrap_or(default_toolset);
            let custom_tools = model_cfg.and_then(|m| m.tools.as_ref());

            let allowed = toolset_allowed_names(toolset, custom_tools);
            if let Some(ref names) = allowed {
                all.retain(|t| names.contains(&t.name.as_str().to_owned()));
            }
            // else: "full" or unknown -> keep all

            // cap-followup sessions: agent is meant to briefly summarise one
            // or more cap completions whose results were already pushed to
            // the user via push_notif. Strip research/exec/chain tools so it
            // can't go web_search-ing the result text (wastes time + floods
            // rate-limited IM channels) or dispatch yet another cap/task.
            if session_key.ends_with(":cap-followup") {
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
                all.retain(|t| !FOLLOWUP_BLOCKED.contains(&t.name.as_str()));
            }

            // Group chat safety: strip dangerous tools to prevent exec via LLM
            let is_group = session_key.contains(":group:");
            if is_group {
                const GROUP_BLOCKED_TOOLS: &[&str] = &[
                    "shell",
                    "execute_command",
                    "exec",
                    "read_file",
                    "read",
                    "write_file",
                    "write",
                    "computer_use",
                ];
                all.retain(|t| !GROUP_BLOCKED_TOOLS.contains(&t.name.as_str()));
            }

            // Auto-tick sessions (heartbeat/system): only memory tool.
            // Cron is excluded — cron-fired agentTurn needs full tool set.
            if is_minimal_context_session(session_key) {
                const INTERNAL_ALLOWED: &[&str] = &["memory"];
                all.retain(|t| INTERNAL_ALLOWED.contains(&t.name.as_str()));
            }

            // Channel-specific tool filtering: only keep the *_actions tool
            // that matches the current channel, strip all others (~500 tokens
            // saved per call).
            const CHANNEL_ACTION_TOOLS: &[&str] = &[
                "telegram_actions",
                "discord_actions",
                "slack_actions",
                "whatsapp_actions",
                "feishu_actions",
                "weixin_actions",
                "qq_actions",
                "dingtalk_actions",
            ];
            // Detect channel type from session_key format:
            //   "agent:<id>:<channel>:direct:<peer>" or "test:api:..."
            let active_channel = session_key.split(':').nth(2).unwrap_or("");
            all.retain(|t| {
                let name = t.name.as_str();
                if CHANNEL_ACTION_TOOLS.contains(&name) {
                    // Keep only if it matches the active channel, or keep the
                    // consolidated "channel_actions" tool.
                    name == "channel_actions" || name.starts_with(active_channel)
                } else {
                    true
                }
            });

            // Cold-tool deferral — non-rsclaw providers only. rsclaw's tools
            // ride in the fleet's shared registry prefix where the KV cache
            // makes them free at the margin; external providers pay the full
            // tool defs on every request, so near-zero-usage tools (~2.8k
            // tokens) collapse into the one-line `request_tool` stub. A
            // per-agent `tools` whitelist is the user's explicit word — never
            // defer those. Tools re-enabled earlier in this session stay live
            // (including ones outside the agent's toolset preset — the stub
            // is deliberately an escape hatch over the full cold pool).
            let primary_provider = model_cfg
                .and_then(|m| m.primary_head())
                .map(|m| self.providers.resolve_model(m).0.to_owned())
                .unwrap_or_default();
            if primary_provider != "rsclaw" && custom_tools.is_none() && !cold_pool.is_empty() {
                let enabled = self
                    .handle
                    .cold_enabled
                    .read()
                    .ok()
                    .and_then(|g| g.get(session_key).cloned())
                    .unwrap_or_default();
                let deferred_names: Vec<&str> = cold_pool
                    .iter()
                    .map(|t| t.name.as_str())
                    .filter(|n| !enabled.contains(*n))
                    .collect();
                if !deferred_names.is_empty() {
                    all.retain(|t| !deferred_names.contains(&t.name.as_str()));
                    for n in &deferred_names {
                        stub_entries.push(((*n).to_owned(), (*n).to_owned()));
                    }
                }
                // Session-enabled cold tools that the toolset filter dropped
                // (or that were just deferred): make sure their real defs are
                // live.
                for t in &cold_pool {
                    if enabled.contains(&t.name) && !all.iter().any(|x| x.name == t.name) {
                        all.push(t.clone());
                    }
                }
                deferred_tool_defs.extend(
                    cold_pool
                        .iter()
                        .filter(|t| !enabled.contains(&t.name))
                        .map(|t| (t.name.clone(), t.clone())),
                );
            }

            // One stub covers both deferred tiers (cold builtins on
            // non-rsclaw providers + plugin tool groups on every provider).
            if !stub_entries.is_empty() {
                all.push(crate::tools_builder::request_tool_def(&stub_entries));
            }

            all
        };

        // Cache tools for compaction KV cache reuse.
        self.cached_tools = tools.clone();

        // DEBUG: when RSCLAW_DUMP_PROMPT is set, dump a JSON document
        // describing this turn's prompt + tool list, split into the
        // shared (cacheable across all RsClaw clients of this version)
        // and user (per-machine) halves. Lets an upstream LLM gateway
        // (rsclaw-llm with kvCacheMode=2) seed its global cache with
        // the shared bytes once per version and dedupe across users.
        // Per session_key in the filename so multiple inspected
        // sessions don't clobber.
        if std::env::var("RSCLAW_DUMP_PROMPT").is_ok() {
            let safe_key = session_key
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>();
            let dump_path = rsclaw_config::loader::base_dir()
                .join(format!("debug_prompt_spec.{safe_key}.json"));

            let tool_json = |t: &rsclaw_provider::ToolDef| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            };
            let mut builtin_tools = Vec::new();
            let mut user_tools = Vec::new();
            for t in &tools {
                if crate::prompt_builder::BUILTIN_TOOL_NAMES.contains(&t.name.as_str()) {
                    builtin_tools.push(tool_json(t));
                } else {
                    user_tools.push(tool_json(t));
                }
            }

            let shared_prefix = crate::prompt_builder::build_shared_system_prefix();
            // user_system is what's left after the shared prefix +
            // "\n\n". Recompute by trimming since the prompt was built
            // by concatenation with that exact separator.
            let user_system = system_prompt
                .strip_prefix(&shared_prefix)
                .map(|rest| rest.trim_start_matches("\n\n").to_owned())
                .unwrap_or_else(|| system_prompt.clone());

            let payload = serde_json::json!({
                "session_key": session_key,
                "agent_id": self.handle.id,
                "model": model,
                "rsclaw_version": env!("CARGO_PKG_VERSION"),
                // SHARED: cacheable, byte-identical for every client of this version.
                "shared_prefix": shared_prefix,
                "builtin_tools": builtin_tools,
                // USER: per-machine, per-session.
                "user_system": user_system,
                "user_tools": user_tools,
                // Convenience: full reconstructed prompt.
                "system_prompt": system_prompt,
            });
            match serde_json::to_string_pretty(&payload) {
                Ok(s) => {
                    if let Err(e) = std::fs::write(&dump_path, &s) {
                        tracing::warn!("failed to dump prompt-spec: {e}");
                    } else {
                        tracing::info!(
                            path = %dump_path.display(),
                            builtin_tool_count = builtin_tools.len(),
                            user_tool_count = user_tools.len(),
                            shared_prefix_len = shared_prefix.len(),
                            user_system_len = user_system.len(),
                            "dumped prompt-spec JSON"
                        );
                    }
                }
                Err(e) => tracing::warn!("prompt-spec serialize failed: {e}"),
            }
        }

        // Check vision support before loading session (avoids borrow conflict).
        let kv_mode = self
            .live
            .agents
            .read()
            .await
            .defaults
            .kv_cache_mode
            .unwrap_or(1);
        // Always detect vision capability — used to decide which model describes
        // images. kvCacheMode >= 1: images are described then stored as text
        // (never base64 in session). kvCacheMode = 0: images kept as base64 in
        // session for vision models.
        let model_has_vision = model_supports_vision(&model, &self.config);
        let _vision = if kv_mode >= 1 {
            false
        } else {
            model_has_vision
        };

        // ---------------------------------------------------------------
        // Media processing: convert images/videos to text descriptions.
        // Done BEFORE load_session() to avoid borrow conflicts with self.
        // Session stores ONLY text — no base64, no binary blobs.
        // This preserves KV cache and prevents context bloat.
        // ---------------------------------------------------------------
        let mut media_descriptions: Vec<String> = Vec::new();
        let mut vision_images_for_current_turn = Vec::<String>::new(); // base64 URIs for vision model

        // Image dispatch. Two paths funnel images into `images` upstream:
        //   1. `extract_file_refs` parses `[file:/abs/path]` markers (desktop drag-drop
        //      / paste path-references) and stores base64-encoded bytes in
        //      `IncomingMsg.images` server-side.
        //   2. `resolve_file_refs` parses `@<src>_<kind>_<id>.<ext>` markers and
        //      produces `resolved.image_paths` (legacy uploads).
        // For the current turn we decide where the bytes go:
        //   - If the primary model is vision-capable: pass-through into
        //     `vision_images_for_current_turn` so it sees the image directly. Most
        //     accurate; fast.
        //   - If the primary model is text-only: fan out to the agent's `vision` slot
        //     for a caption, then inject the caption as a text media-description. The
        //     primary then runs against text only — saves base64 on the wire, keeps KV
        //     cache hot, and stops the silent-hallucination failure mode where the
        //     primary model receives image_url content it can't read and either ignores
        //     it (faking a description) or keyword-routes to a tool whose name happens
        //     to match the filename (e.g. `computer_use action=screenshot` for
        //     `screenshot.png`).
        //   - On caption failure (no vision chain configured, timeout, provider error)
        //     fall back to pass-through. Worse than a successful caption, but better
        //     than dropping the image.
        if !images.is_empty() {
            if model_has_vision {
                for img in &images {
                    vision_images_for_current_turn.push(img.data.clone());
                }
            } else {
                match self
                    .caption_images_for_text_only_primary(text, &images)
                    .await
                {
                    Ok(caption) => {
                        tracing::info!(
                            n_images = images.len(),
                            caption_len = caption.len(),
                            primary = %model,
                            "vision-as-tool: captioned images for text-only primary"
                        );
                        media_descriptions.push(format!(
                            "[Image(s) attached — vision-model description below; \
                             original image(s) not forwarded to primary]\n{}",
                            caption.trim()
                        ));
                    }
                    Err(e) => {
                        // DO NOT pass-through to a text-only primary —
                        // that ships ~85k tokens of base64 to a model
                        // that can't read images, which then hallucinates
                        // (or hangs streaming). Tell primary the image
                        // exists and that vision failed; let it ask the
                        // user instead of guessing.
                        tracing::warn!(
                            error = %e,
                            n_images = images.len(),
                            primary = %model,
                            "vision-as-tool: caption failed; surfacing as text-only error"
                        );
                        // Surface source paths when we have them so the user
                        // can re-attach or retry with a different tool —
                        // base64 alone is useless to them.
                        let path_hint: String = {
                            let paths: Vec<&str> = images
                                .iter()
                                .filter_map(|img| img.source_path.as_deref())
                                .collect();
                            if paths.is_empty() {
                                String::new()
                            } else {
                                format!(" (path: {})", paths.join(", "))
                            }
                        };
                        media_descriptions.push(format!(
                            "User uploaded {n} image(s){path_hint} but vision captioning failed ({e}). \
                             Tell the user the image could not be parsed. Suggest retrying later, \
                             or re-referencing the same path with a different tool.",
                            n = images.len()
                        ));
                    }
                }
            }
        }

        // (Image → FileAttachment conversion already done above, before file
        // processing.)

        // Build the persisted message: user text + media descriptions (text only).
        let persist_text = if media_descriptions.is_empty() {
            text.to_owned()
        } else {
            format!("{}\n\n{}", text, media_descriptions.join("\n"))
        };

        // NOW load session (after media processing is done, no more self borrows).
        // Internal sessions (heartbeat/cron/system) should start each tick
        // with fresh state — drop any in-memory history from the previous
        // tick before loading (DB is never written for these sessions, so
        // load_session will return an empty Vec).
        if is_internal {
            self.sessions.remove(session_key);
        }
        let session_messages = self.load_session(session_key);

        // First user message in session: prepend session metadata (date, timezone,
        // channel). Stored in session so it becomes part of the stable prefix
        // for KV cache — never changes across turns.
        // Also triggers after /clear (session may contain a summary but no user
        // messages).
        let has_user_msg = session_messages.iter().any(|m| m.role == Role::User);
        // Fresh, per-turn wall-clock. The old code stamped the date ONCE on the
        // first message and froze it into the session prefix "for KV cache
        // stability" — so a session opened on day N still reported day N's date
        // a week later, and the model reasoned about the wrong day (e.g. "it's
        // 6/24" on 7/1). The date line lives in the session-specific USER
        // message, not the cross-user system prompt, so the shared prefix that
        // kvCache=2 dedupes is untouched. Messages carry no timestamp of their
        // own in the store, so persisting this line also timestamps history.
        let date_ctx = crate::prompt_builder::build_date_context();
        let persist_text = if !has_user_msg {
            let now = chrono::Local::now();
            let tz = now.format("%Z").to_string();
            let session_meta = format!(
                "[Session started: {} {}, {}, via {}]",
                now.format("%Y-%m-%d %H:%M"),
                now.format("%A"),
                tz,
                channel,
            );
            format!("{date_ctx}\n{session_meta}\n{persist_text}")
        } else {
            format!("{date_ctx}\n{persist_text}")
        };

        let persist_msg = Message {
            role: Role::User,
            content: MessageContent::Text(persist_text),
            // Hidden replay state is a NATIVE protocol concept — external
            // providers must never persist rsclaw_hidden they can't consume
            // (they get the bundle as request-local text instead).
            rsclaw_hidden: if model_provider == "rsclaw" {
                auto_recall_bundle
                    .as_ref()
                    .and_then(RecallBundle::to_rsclaw_hidden)
            } else {
                None
            },
        };
        session_messages.push(persist_msg.clone());
        // Internal sessions (heartbeat/cron/system): skip DB persist —
        // each tick is independent and we don't want history accumulating
        // "HEARTBEAT_OK" replies in redb.
        if !is_internal {
            if let Err(e) = self.store.db.append_message(
                session_key,
                &serde_json::to_value(&persist_msg).unwrap_or_default(),
            ) {
                tracing::warn!("failed to persist user message: {e:#}");
            }
        }

        // Timeout wrapper. Daemon agents (long-lived monitor loops) run with NO
        // turn timeout — they loop forever by design; see `daemon_agent_ids`.
        let daemon_mode: bool = self.config.agents.is_daemon_agent(&self.handle.id);
        let timeout_secs = self
            .config
            .agents
            .defaults
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS as u32) as u64;

        // Get or create abort flag for this session.
        let abort_flag: Arc<AtomicBool> = {
            let mut flags = self
                .handle
                .abort_flags
                .write()
                .expect("abort_flags lock poisoned");
            Arc::clone(
                flags
                    .entry(session_key.to_string())
                    .or_insert_with(|| Arc::new(AtomicBool::new(false))),
            )
        };

        // RAII guard: clears abort flag when turn exits (normal or error).
        let _guard = AbortFlagGuard {
            handle: Arc::clone(&self.handle),
            session_key: session_key.to_string(),
        };

        // Check if abort was requested before starting.
        if abort_flag.load(Ordering::SeqCst) {
            abort_flag.store(false, Ordering::SeqCst);
            return Ok(AgentReply {
                text: "[aborted]".to_string(),
                is_empty: false,
                tool_calls: None,
                images: vec![],
                files: vec![],
                pending_analysis: None,
                // Pre-loop abort bypasses agent_loop.
                needs_outer_done_emit: true,
                outcome: crate::registry::ReplyOutcome::Ok,
            });
        }

        let mut ctx = RunContext {
            agent_id: self.handle.id.clone(),
            session_key: session_key.to_owned(),
            channel: channel.to_owned(),
            peer_id: peer_id.to_owned(),
            // Channel/group ID for the inbound message. Notification routing
            // and tool callbacks fall back to peer_id when this is empty,
            // which on Discord groups produces a 404 (Discord rejects POST
            // to /channels/<user_id>/messages — DMs need a created channel).
            chat_id: chat_id.to_owned(),
            account: account.map(str::to_owned),
            exec_pool: Arc::clone(&self.exec_pool),
            loop_detector: {
                let ld_cfg_owned = self
                    .live
                    .ext
                    .read()
                    .await
                    .tools
                    .as_ref()
                    .and_then(|t| t.loop_detection.clone());
                let ld_cfg = ld_cfg_owned.as_ref();
                if ld_cfg.map(|c| c.enabled.unwrap_or(true)).unwrap_or(true) {
                    let window = ld_cfg.and_then(|c| c.window).unwrap_or(20);
                    let warning_threshold = ld_cfg.and_then(|c| c.threshold).unwrap_or(20);
                    let critical_threshold = warning_threshold
                        .saturating_add(10)
                        .max(warning_threshold + 1);
                    let overrides: std::collections::HashMap<String, (usize, usize)> = ld_cfg
                        .and_then(|c| c.overrides.clone())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(k, v)| (k, (v, v.saturating_add(10).max(v + 1))))
                        .collect();
                    LoopDetector::with_overrides(
                        window,
                        warning_threshold,
                        critical_threshold,
                        overrides,
                    )
                } else {
                    LoopDetector::new(usize::MAX, usize::MAX)
                }
            },
            has_images: !vision_images_for_current_turn.is_empty(),
            user_msg_with_images: if !vision_images_for_current_turn.is_empty() {
                // Build a multimodal message for the current LLM turn only.
                // The persisted message is text-only; this adds images back for vision.
                let base_text = match &persist_msg.content {
                    MessageContent::Text(t) => t.clone(),
                    MessageContent::Parts(p) => p
                        .iter()
                        .filter_map(|part| {
                            if let ContentPart::Text { text } = part {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                };
                let mut parts = vec![ContentPart::Text { text: base_text }];
                for img_uri in &vision_images_for_current_turn {
                    parts.push(ContentPart::Image {
                        url: img_uri.clone(),
                    });
                }
                Some(Message {
                    role: Role::User,
                    content: MessageContent::Parts(parts),
                    rsclaw_hidden: persist_msg.rsclaw_hidden.clone(),
                })
            } else {
                None
            },
            parse_error_count: 0,
            recalled_memory_ids: auto_recalled_ids,
            auto_recall: auto_recall_bundle,
            loop_warning_triggered: false,
            loop_failure: None,
            turn_metrics: crate::turn_metrics::TurnMetrics::new(),
            user_text: text.to_owned(),
            full_trace: init_full_trace(text),
            turn_ctx,
        };

        // Plugins + Skills rendering now lives inside `build_user_system`
        // (called when assembling `system_prompt` above). No separate
        // Role::System injection / cached_*_system / new_skills_tail
        // diff path needed: every turn rebuilds user_system from the
        // current SkillRegistry + plugin state, so freshly installed
        // skills show up in the next turn's `## Installed Skills`
        // section automatically. Worker-side KV layer-2 cache re-prefills
        // when the user_system bytes change (i.e. install/uninstall),
        // which is the correct behaviour.

        let agent_loop_fut = self.agent_loop(
            &mut ctx,
            &model,
            primary_chain_tail.clone(),
            &system_prompt,
            tools,
            deferred_tool_defs,
            extra_tools,
            abort_flag.clone(),
        );
        let reply = if daemon_mode {
            // No turn timeout for daemon agents — they loop indefinitely.
            agent_loop_fut.await?
        } else {
            time::timeout(Duration::from_secs(timeout_secs), agent_loop_fut)
                .await
                .map_err(|_| {
                    anyhow!(
                        "agent `{}` turn timed out after {timeout_secs}s",
                        self.handle.id
                    )
                })??
        };

        // Update live status: turn finished.
        if let Ok(mut status) = self.live_status.try_write() {
            status.state = "idle".to_owned();
            status.current_task.clear();
            status.text_preview.clear();
        }
        self.handle
            .session_count
            .store(self.sessions.len(), std::sync::atomic::Ordering::Relaxed);

        // Append to JSONL transcript (AGENTS.md §20 step 11).
        self.append_transcript(session_key, text, &reply.text).await;

        // Loop A (organic evolution): adjust importance of recalled memories
        // based on the outcome of this turn.
        tracing::debug!(
            recalled_count = ctx.recalled_memory_ids.len(),
            loop_warning = ctx.loop_warning_triggered,
            reply_empty = reply.is_empty,
            "evolution: feedback check"
        );
        if let Some(ref mem) = self.memory
            && !ctx.recalled_memory_ids.is_empty()
        {
            let signal = Self::infer_outcome_signal(&reply, &ctx, channel);
            tracing::debug!(
                signal,
                recalled = ctx.recalled_memory_ids.len(),
                "evolution: applying feedback"
            );
            if signal.abs() > f32::EPSILON {
                let mut store = mem.lock().await;
                for mem_id in &ctx.recalled_memory_ids {
                    if let Err(e) = store.adjust_importance(mem_id, signal).await {
                        tracing::debug!(mem_id, "evolution feedback adjust: {e:#}");
                    }
                }
            }
        }

        // Loop B (organic evolution): check if any recalled memory just promoted
        // to Core, and if so, spawn a background crystallization attempt.
        if let Some(ref mem) = self.memory
            && !ctx.recalled_memory_ids.is_empty()
        {
            let store = mem.lock().await;
            let candidates: Vec<String> = ctx
                .recalled_memory_ids
                .iter()
                .filter(|id| {
                    store
                        .get_sync(id)
                        .map(|d| {
                            d.tier == crate::memory::MemDocTier::Core
                                && !d.tags.iter().any(|t| t == "crystallized")
                        })
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            drop(store);

            if !candidates.is_empty() {
                let mem_clone = Arc::clone(mem);
                let providers = Arc::clone(&self.providers);
                let model = self.resolve_flash_model_name();
                // Crystallized skills go to the global skill directory so the
                // existing load_skills() call sites pick them up on next reload.
                let skills_dir = rsclaw_skill::default_global_skills_dir()
                    .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("skills"));
                let scope = format!("agent:{}", self.handle.id);
                tokio::spawn(async move {
                    for doc_id in candidates {
                        if let Err(e) = rsclaw_skill::crystallizer::crystallize_one(
                            &mem_clone,
                            &doc_id,
                            &scope,
                            &providers,
                            &model,
                            &skills_dir,
                        )
                        .await
                        {
                            tracing::warn!(doc_id, "crystallize_one hard failure: {e:#}");
                        }
                    }
                });
            }
        }

        // Auto-Capture (see docs/memory-extraction-redesign.md).
        // Phase 1 — stop the bleeding: we NO LONGER persist every user turn
        // verbatim as a `note`. That made the store a chat log: "在吗?",
        // injected banner prompts, and test instructions were all dumped in as
        // kind=note/tier=working with zero distillation. Only deterministic
        // entity extraction (phone/ID/email → pinned `entity` memories) runs
        // here now. Distilled fact/preference/procedure extraction lands in the
        // L1 extractor in a later phase. Key data like "手机号18674030927" is
        // still captured — via the entity path, which is more precise than a
        // raw note ever was.
        //
        // Gated by `agents.defaults.memory.autoCapture` (default on). The flag
        // used to be defined-but-never-read; this is the first reader, so
        // setting it false now actually disables auto-capture.
        let auto_capture_enabled = self
            .config
            .agents
            .defaults
            .memory
            .as_ref()
            .and_then(|m| m.auto_capture)
            .unwrap_or(true);
        // Skip auto-capture for internal channels — heartbeat/cron/system
        // don't need long-term memory and would pollute user recall results.
        let internal_channel = matches!(channel, "heartbeat" | "cron" | "system");
        if let Some(ref mem) = self.memory
            && auto_capture_enabled
            && text.len() > 8
            && !reply.text.starts_with(NO_REPLY_TOKEN)
            && !internal_channel
        {
            let doc_scope = format!("agent:{}", self.handle.id);

            // Deterministic entity extraction: phone numbers, ID cards, emails.
            // These become proper pinned `entity` memories — not raw notes.
            let user_entities = crate::context_mgr::extract_key_entities(text);
            if !user_entities.is_empty() {
                let docs =
                    crate::context_mgr::write_entity_memories(mem, &doc_scope, user_entities).await;
                for doc in docs {
                    if let Err(e) = self
                        .store
                        .search
                        .index_memory_doc(&doc.id, &doc.scope, &doc.kind, &doc.text)
                    {
                        tracing::warn!("BM25 index failed for auto-captured entity: {e:#}");
                    }
                }
            }

            // L1 extraction (docs/memory-extraction-redesign.md, Phase 3):
            // distill soft durable signal — preferences, identity, procedures,
            // relationships, project state — that the deterministic pass above
            // can't see. Gated by a cheap salience check so chit-chat / task
            // requests never hit the LLM, and spawned so the flash call never
            // delays this turn's reply.
            if crate::memory_extractor::salience_gate(text) {
                let mem_clone = Arc::clone(mem);
                let providers = Arc::clone(&self.providers);
                // Use the flash model for structured extraction — the prompt
                // is a templated JSON-array instruction that doesn't need
                // primary-tier reasoning. Falls back to primary when no flash
                // model is configured.
                let model = self.resolve_flash_model_name();
                let scope = doc_scope.clone();
                let user_text = text.to_owned();
                tokio::spawn(async move {
                    crate::memory_extractor::extract_l1(
                        mem_clone, providers, model, scope, user_text,
                    )
                    .await;
                });
            }

            // Lesson extraction: when the user message looks like a correction
            // or a durable behavioral instruction, distill it into a `lesson`
            // memory (Core tier) so the same mistake isn't repeated. Same
            // user-message-only trust boundary and spawn/best-effort shape as
            // L1; separate gate so it fires on corrections L1's gate misses.
            if crate::memory_extractor::correction_gate(text) {
                let mem_clone = Arc::clone(mem);
                let providers = Arc::clone(&self.providers);
                let model = self.resolve_flash_model_name();
                let scope = doc_scope.clone();
                let user_text = text.to_owned();
                tokio::spawn(async move {
                    crate::memory_extractor::extract_lesson(
                        mem_clone, providers, model, scope, user_text,
                    )
                    .await;
                });
            }

            // Failure-lesson extraction: if the agent loop got stuck repeating a
            // tool call this turn, distill a generalizable lesson from the task
            // + the factual loop trace (kind=failure, Working tier). Triggered
            // only by the hard loop signal, so transient single calls don't
            // create noise.
            if let Some(failure_trace) = ctx.loop_failure.clone() {
                let mem_clone = Arc::clone(mem);
                let providers = Arc::clone(&self.providers);
                let model = self.resolve_flash_model_name();
                let scope = doc_scope.clone();
                let user_text = text.to_owned();
                tokio::spawn(async move {
                    crate::memory_extractor::extract_failure_lesson(
                        mem_clone,
                        providers,
                        model,
                        scope,
                        user_text,
                        failure_trace,
                    )
                    .await;
                });
            }

            // Note: structured-entity extraction from the compaction summary
            // still runs at compaction time (zero extra LLM calls there); L1
            // above is the per-turn complement for the soft kinds.
        }

        // NOTE: We intentionally do NOT extract entities from reply.text.
        // Harvesting "facts" from the agent's own output causes hallucinations
        // (e.g. a fabricated third-person success narrative) to be crystallized
        // into entity memory and then fed back on the next turn, reinforcing
        // the false belief. Entities are extracted only from user messages and
        // tool outputs (both trusted sources).

        // Compaction check (AGENTS.md §15).
        self.compact_if_needed(session_key, &model).await;

        // Evict stale sessions if the cache has grown too large.
        self.evict_stale_sessions();

        // Auto-TTS: if session is in voice mode, generate audio for the reply.
        let mut reply = reply;
        if self.voice_mode_sessions.contains(session_key)
            && !reply.text.is_empty()
            && !reply.is_empty
            && !reply.needs_outer_done_emit
        {
            // One-shot install hint: when sherpa-onnx is missing the TTS
            // path falls back to system `say` / SAPI / espeak which sound
            // robotic for Chinese. Surface the install command once via
            // the reply.text — `claim_first_hint` returns true only on
            // the first call per feature so this fires exactly once.
            let sherpa_tts_bin = rsclaw_config::loader::base_dir()
                .join("tools")
                .join("sherpa-onnx")
                .join("bin")
                .join(if cfg!(target_os = "windows") {
                    "sherpa-onnx-offline-tts.exe"
                } else {
                    "sherpa-onnx-offline-tts"
                });
            let has_vits_dir = std::fs::read_dir(rsclaw_config::loader::base_dir().join("models"))
                .map(|entries| {
                    entries.flatten().any(|e| {
                        e.path().is_dir() && e.file_name().to_string_lossy().starts_with("vits-")
                    })
                })
                .unwrap_or(false);
            let sherpa_tts_ready = sherpa_tts_bin.exists() && has_vits_dir;
            if !sherpa_tts_ready && crate::install_hints::claim_first_hint("tts-sherpa") {
                let lang = rsclaw_i18n::default_lang();
                reply
                    .text
                    .push_str(&rsclaw_i18n::t("install_hint_tts_sherpa", lang));
            }

            match self.generate_tts_audio(&reply.text).await {
                Ok(audio_path) => {
                    let mime = if audio_path.ends_with(".wav") {
                        "audio/wav"
                    } else if audio_path.ends_with(".mp3") {
                        "audio/mpeg"
                    } else {
                        "audio/wav"
                    };
                    reply.files.push((
                        std::path::Path::new(&audio_path)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "reply.wav".to_owned()),
                        mime.to_owned(),
                        audio_path,
                    ));
                    debug!(session = session_key, "auto-TTS audio attached to reply");
                }
                Err(e) => {
                    warn!(session = session_key, "auto-TTS failed: {e:#}");
                }
            }
        }

        // Plugin hook: after_turn (AGENTS.md §20).
        self.fire_hook(
            "after_turn",
            json!({
                "agent_id": self.handle.id,
                "session_key": session_key,
                "reply_len": reply.text.len(),
                "is_empty": reply.is_empty,
            }),
        )
        .await;

        Ok(reply)
    }
}
