//! Session compaction — LLM-based context summarization and transcript logging.

use std::sync::Arc;

use chrono::Utc;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;
use tracing::{debug, info, warn};

use super::context_mgr::{compress_tool_results, estimate_tokens, msg_tokens};
use super::runtime::AgentRuntime;
use crate::provider::{
    ContentPart, LlmRequest, Message, MessageContent, Role, StreamEvent,
};

impl AgentRuntime {
    /// Summarise the session history via LLM when the total character count
    /// approaches `reserveTokensFloor` (approximated as floor * 4 chars/token,
    /// default 100 000 chars).
    ///
    /// **Layered mode** (default): keeps the last N user-assistant pairs
    /// verbatim and only summarises the older portion, so recent context is
    /// never lost.  Falls back to Default/Safeguard when configured.
    pub(crate) async fn compact_if_needed(&mut self, session_key: &str, model: &str) {
        self.compact_inner(session_key, model, false).await;
    }

    /// Force compaction regardless of threshold (used by /compact).
    pub(crate) async fn compact_force(&mut self, session_key: &str, model: &str) {
        self.compact_inner(session_key, model, true).await;
    }

    pub(crate) async fn compact_inner(&mut self, session_key: &str, model: &str, force: bool) {
        use crate::config::schema::CompactionMode;

        // Use configured compaction settings, or sensible defaults.
        let cfg = self.config.agents.defaults.compaction.clone()
            .unwrap_or_default();

        // Compaction trigger: token threshold ONLY.
        // Turn count and time-based triggers were removed because they
        // unnecessarily discard context and break KV cache in the new
        // append-only architecture.
        let context_tokens = self.config.agents.defaults.context_tokens.unwrap_or(64_000) as usize;
        let kv_cache_mode = self.config.agents.defaults.kv_cache_mode.unwrap_or(1);
        // kvCacheMode >= 1: append-only mode, delay compaction to 95%
        // to maximize KV cache reuse. Mode 0: legacy 80% threshold.
        let default_threshold = if kv_cache_mode >= 1 {
            (context_tokens * 19 / 20).max(16_000) // 95%
        } else {
            (context_tokens * 4 / 5).max(16_000)   // 80%
        };
        let token_threshold = cfg
            .reserve_tokens_floor
            .map(|t| t as usize)
            .unwrap_or(default_threshold);

        let total_tokens: usize = self
            .sessions
            .get(session_key)
            .map(|msgs| msgs.iter().map(msg_tokens).sum())
            .unwrap_or(0);

        let turns = self
            .compaction_state
            .get(session_key)
            .map(|(_, t)| *t)
            .unwrap_or(0);

        let token_trigger = total_tokens > token_threshold;

        debug!(
            session = session_key,
            total_tokens,
            token_threshold,
            turns,
            token_trigger,
            force,
            "compaction check"
        );

        if !force && !token_trigger {
            self.compaction_state
                .entry(session_key.to_owned())
                .and_modify(|(_, t)| *t += 1)
                .or_insert((std::time::Instant::now(), 1));
            return;
        }

        let trigger_reason = if token_trigger {
            "tokens"
        } else {
            "time"
        };
        info!(
            session = session_key,
            trigger = trigger_reason,
            total_tokens,
            turns,
            "compaction triggered"
        );

        let mode = cfg
            .mode
            .as_ref()
            .cloned()
            .unwrap_or(CompactionMode::Layered);
        let compaction_model = cfg.model.as_deref().unwrap_or(model);
        // Dynamic keepRecentPairs: reduce when token pressure is high.
        let configured_pairs = cfg.keep_recent_pairs.unwrap_or(5) as usize;
        let keep_pairs = if total_tokens > token_threshold * 3 {
            1.max(configured_pairs / 3) // extreme pressure: keep 1-2 pairs
        } else if total_tokens > token_threshold * 2 {
            1.max(configured_pairs / 2) // high pressure: keep 2-3 pairs
        } else {
            configured_pairs // normal: use configured value
        };
        let extract_facts = cfg.extract_facts.unwrap_or(true);

        let msgs_to_text = |msgs: &[Message]| -> String {
            let default_transcript = (context_tokens * 7 / 10).max(16_000);
            let max_total_tokens: usize = cfg.max_transcript_tokens
                .map(|t| t as usize)
                .unwrap_or(default_transcript);
            Self::msgs_to_text_static(msgs, max_total_tokens)
        };

        // Split messages into (old_portion, recent_portion) for layered mode.
        let (old_text, recent_msgs) = if mode == CompactionMode::Layered {
            let msgs = self.sessions.get(session_key).cloned().unwrap_or_default();
            // Count user-assistant pairs from the end.
            let mut pair_count = 0usize;
            let mut split_idx = msgs.len();
            let mut i = msgs.len();
            while i > 0 && pair_count < keep_pairs {
                i -= 1;
                if msgs[i].role == Role::User {
                    pair_count += 1;
                    split_idx = i;
                }
            }
            let mut old_portion = msgs[..split_idx].to_vec();
            let recent = msgs[split_idx..].to_vec();
            if old_portion.is_empty() {
                // Not enough history to compact -- skip.
                return;
            }
            // Compress verbose tool results before LLM summarization to
            // reduce input size. Preserves the last 6 messages in the old
            // portion (3 user-assistant pairs) for continuity.
            compress_tool_results(&mut old_portion, 6);
            (msgs_to_text(&old_portion), recent)
        } else {
            let mut msgs = self.sessions.get(session_key).cloned().unwrap_or_default();
            compress_tool_results(&mut msgs, 6);
            (msgs_to_text(&msgs), vec![])
        };

        // Pre-compaction entity preservation: deterministic extraction (phone/ID/email)
        // plus LLM-based semantic extraction (name, birthday, zodiac, etc.)
        // Both run BEFORE the LLM summary to guarantee no data loss.
        {
            let entities = crate::agent::context_mgr::extract_key_entities(&old_text);
            if !entities.is_empty() {
                if let Some(ref mem) = self.memory {
                    let scope = format!("agent:{}", self.handle.id);
                    crate::agent::context_mgr::write_entity_memories(mem, &scope, entities).await;
                    debug!(session = session_key, "pre-compaction deterministic entities pinned");
                }
            }
            // LLM-based semantic entity extraction before compaction.
            if let Some(ref mem) = self.memory {
                let scope = format!("agent:{}", self.handle.id);
                let llm_entities = crate::agent::context_mgr::extract_entities_via_llm(
                    &old_text, compaction_model, &mut self.failover, &self.providers,
                ).await;
                if !llm_entities.is_empty() {
                    crate::agent::context_mgr::write_entity_memories(mem, &scope, llm_entities).await;
                    debug!(session = session_key, "pre-compaction LLM entities pinned");
                }
            }
        }

        // Summarise the old portion.
        let summary = match mode {
            CompactionMode::Default | CompactionMode::Layered => {
                self.compact_single(compaction_model, &old_text).await
            }
            CompactionMode::Safeguard => {
                const CHUNK_SIZE: usize = 40_000;
                // Split at char boundaries to avoid breaking multi-byte UTF-8.
                let chunks: Vec<&str> = {
                    let mut result = Vec::new();
                    let mut remaining = old_text.as_str();
                    while !remaining.is_empty() {
                        let mut end = CHUNK_SIZE.min(remaining.len());
                        while end < remaining.len() && !remaining.is_char_boundary(end) {
                            end -= 1;
                        }
                        let (chunk, rest) = remaining.split_at(end);
                        result.push(chunk);
                        remaining = rest;
                    }
                    result
                };
                let mut combined = String::new();
                for chunk in chunks {
                    match self.compact_single(compaction_model, chunk).await {
                        Some(s) => {
                            combined.push_str(&s);
                            combined.push('\n');
                        }
                        None => return,
                    }
                }
                if combined.is_empty() {
                    None
                } else {
                    Some(combined)
                }
            }
        };

        let Some(summary) = summary else { return };

        // -- Key fact extraction: store important facts in long-term memory --
        if extract_facts {
            if let Some(facts) = self.extract_key_facts(compaction_model, &old_text).await {
                if let Some(ref mem) = self.memory {
                    let scope = format!("agent:{}", self.handle.id);
                    let mut guard = mem.lock().await;
                    for fact in facts.lines().filter(|l| !l.trim().is_empty()) {
                        let fact_text = fact.trim_start_matches("- ").trim();
                        if fact_text.len() > 5 {
                            let doc = crate::agent::memory::MemoryDoc {
                                id: format!("cf-{}", uuid::Uuid::new_v4()),
                                scope: scope.clone(),
                                kind: "compaction_fact".to_owned(),
                                text: fact_text.to_owned(),
                                vector: vec![],
                                created_at: 0, // filled by add()
                                accessed_at: 0,
                                access_count: 0,
                                importance: 0.7, // higher than default
                                tier: Default::default(),
                                abstract_text: None,
                                overview_text: None,
                                tags: vec![],
                pinned: false,
                            };
                            if let Err(e) = guard.add(doc).await {
                                tracing::warn!("compaction fact memory add failed: {e:#}");
                            }
                        }
                    }
                    drop(guard);
                    debug!(
                        session = session_key,
                        "key facts extracted to long-term memory"
                    );
                }
            }
        }

        // Replace session history: summary + recent turns kept verbatim.
        if let Some(sess) = self.sessions.get_mut(session_key) {
            let compacted = Message {
                role: Role::User,
                content: MessageContent::Text(format!(
                    "[Conversation history compacted — summary follows]\n{summary}"
                )),
            };
            sess.clear();
            sess.push(compacted);
            // Re-append the recent messages that we kept.
            sess.extend(recent_msgs);
        }

        // Reset compaction state after successful compaction.
        self.compaction_state
            .insert(session_key.to_owned(), (std::time::Instant::now(), 0));

        // Persist compacted session to redb (survives restarts).
        if let Some(sess) = self.sessions.get(session_key) {
            if let Err(e) = self.store.db.delete_session(session_key) {
                tracing::warn!("compaction: failed to delete old session: {e:#}");
            }
            for msg in sess.iter() {
                let val = serde_json::to_value(msg).unwrap_or_default();
                if let Err(e) = self.store.db.append_message(session_key, &val) {
                    tracing::warn!("compaction: failed to persist message: {e:#}");
                }
            }
        }

        let new_tokens: usize = self
            .sessions
            .get(session_key)
            .map(|msgs| msgs.iter().map(msg_tokens).sum())
            .unwrap_or(0);
        info!(
            session = session_key,
            tokens_before = total_tokens,
            tokens_after = new_tokens,
            keep_pairs,
            "auto-compaction complete (layered)"
        );

        // If compaction barely helped (still >80% of threshold), inject a
        // system hint so the agent will relay the /reset suggestion to the user.
        if new_tokens > token_threshold * 4 / 5 {
            let zh = crate::i18n::default_lang() == "zh";
            let hint = if zh {
                "[system] 上下文压缩后仍然较大，响应可能变慢。请告知用户发送 /reset 重置会话以恢复正常速度。"
            } else {
                "[system] Context is still large after compaction and responses may slow down. Please tell the user to send /reset to start a fresh session."
            };
            if let Some(sess) = self.sessions.get_mut(session_key) {
                sess.push(Message {
                    role: Role::System,
                    content: MessageContent::Text(hint.to_owned()),
                });
            }
            warn!(
                session = session_key,
                tokens_after = new_tokens,
                threshold = token_threshold,
                "compaction insufficient, /reset recommended"
            );
        }

        // Persist compaction marker to transcript.
        self.append_transcript(
            session_key,
            "[auto-compaction triggered]",
            &format!("[summary: {summary}]"),
        )
        .await;
    }

    /// Render messages as plain text transcript with two-pass budget allocation.
    ///
    /// Total output is capped at `max_total_tokens` to avoid blowing up the
    /// compact LLM's context window. Recent messages get full detail first;
    /// older messages get progressively reduced detail until budget is exhausted.
    pub(crate) fn msgs_to_text_static(msgs: &[Message], max_total_tokens: usize) -> String {
        // Helper: truncate to N chars (UTF-8 safe).
        fn trunc(s: &str, max: usize) -> String {
            match s.char_indices().nth(max) {
                None => s.to_owned(),
                Some((byte_idx, _)) => {
                    let mut t = s[..byte_idx].to_owned();
                    t.push_str("...[truncated]");
                    t
                }
            }
        }

        // Helper: smart-truncate tool_call args.
        fn compact_args(input: &Value) -> String {
            const BULK_FIELDS: &[&str] = &["content", "old_string", "new_string"];
            const MAX_BULK: usize = 300;
            const MAX_CMD: usize = 500;
            const MAX_TOTAL: usize = 2000;

            if let Some(obj) = input.as_object() {
                let needs = obj.iter().any(|(k, v)| {
                    let limit = if BULK_FIELDS.contains(&k.as_str()) { MAX_BULK }
                                else if k == "command" { MAX_CMD }
                                else { return false; };
                    v.as_str().map(|s| s.char_indices().nth(limit).is_some()).unwrap_or(false)
                });
                if needs {
                    let mut compact = serde_json::Map::new();
                    for (k, v) in obj {
                        let limit = if BULK_FIELDS.contains(&k.as_str()) { Some(MAX_BULK) }
                                    else if k == "command" { Some(MAX_CMD) }
                                    else { None };
                        if let (Some(lim), Some(s)) = (limit, v.as_str()) {
                            compact.insert(k.clone(), Value::String(trunc(s, lim)));
                        } else {
                            compact.insert(k.clone(), v.clone());
                        }
                    }
                    let ser = serde_json::to_string(&Value::Object(compact)).unwrap_or_default();
                    return if ser.char_indices().nth(MAX_TOTAL).is_some() { trunc(&ser, MAX_TOTAL) } else { ser };
                }
            }
            let full = serde_json::to_string(input).unwrap_or_default();
            if full.char_indices().nth(MAX_TOTAL).is_some() { trunc(&full, MAX_TOTAL) } else { full }
        }

        // Render a single message at the given detail level:
        //   2 = full (tool args + results), 1 = medium, 0 = minimal
        let render_msg = |m: &Message, detail: u8| -> String {
            let role = format!("{:?}", m.role).to_lowercase();
            let body = match &m.content {
                MessageContent::Text(t) => {
                    if detail == 0 { trunc(t, 200) } else { t.clone() }
                }
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(
                            if detail == 0 { trunc(text, 200) } else { text.clone() }
                        ),
                        ContentPart::ToolUse { name, input, .. } => match detail {
                            2 => Some(format!("[tool_call: {name}({})]", compact_args(input))),
                            1 => Some(format!("[tool_call: {name}({})]",
                                trunc(&serde_json::to_string(input).unwrap_or_default(), 100))),
                            _ => Some(format!("[tool_call: {name}]")),
                        },
                        ContentPart::ToolResult { tool_use_id: _, content, .. } => match detail {
                            2 => Some(format!("[tool_result: {}]", trunc(content, 800))),
                            1 => Some(format!("[tool_result: {}]", trunc(content, 150))),
                            _ => None,
                        },
                        ContentPart::Image { .. } => Some("[image]".to_owned()),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            format!("{role}: {body}")
        };

        // Pass 1: full detail, check if within budget.
        let full: Vec<String> = msgs.iter().map(|m| render_msg(m, 2)).collect();
        let full_tokens: Vec<usize> = full.iter().map(|s| estimate_tokens(s)).collect();
        let total: usize = full_tokens.iter().sum();
        if total <= max_total_tokens {
            return full.join("\n");
        }

        // Pass 2: allocate budget from newest to oldest.
        let n = msgs.len();
        let mut detail_levels = vec![0u8; n];
        let mut budget_used = 0usize;
        for i in (0..n).rev() {
            if budget_used + full_tokens[i] <= max_total_tokens {
                detail_levels[i] = 2;
                budget_used += full_tokens[i];
            } else {
                let m = &msgs[i];
                for &d in &[1u8, 0] {
                    let rendered = render_msg(m, d);
                    let cost = estimate_tokens(&rendered);
                    if budget_used + cost <= max_total_tokens || d == 0 {
                        detail_levels[i] = d;
                        budget_used += cost.min(max_total_tokens.saturating_sub(budget_used));
                        break;
                    }
                }
            }
            if budget_used >= max_total_tokens {
                break;
            }
        }

        // Final render in order.
        let mut result = String::new();
        let mut tokens_used = 0usize;
        for (i, m) in msgs.iter().enumerate() {
            let line = if detail_levels[i] == 2 {
                full[i].clone()
            } else {
                render_msg(m, detail_levels[i])
            };
            let line_tokens = estimate_tokens(&line);
            if tokens_used + line_tokens > max_total_tokens {
                result.push_str("\n...[context truncated]");
                break;
            }
            result.push_str(&line);
            result.push('\n');
            tokens_used += line_tokens;
        }
        result
    }

    /// Call the LLM once with a summarization prompt and return the text.
    pub(crate) async fn compact_single(&mut self, model: &str, history: &str) -> Option<String> {
        let req = LlmRequest {
            model: model.to_owned(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(format!(
                    "Summarize the following conversation into these sections:\n\n\
                     1. **Task & Intent**: What the user wants to accomplish\n\
                     2. **Key Data**: Exact values that MUST be preserved verbatim \
                        (file paths, URLs, cron expressions, config values, IDs, \
                        variable names, port numbers, credentials, phone numbers, \
                        account numbers, any numeric sequence discovered during the conversation)\n\
                     3. **Actions Taken**: Tool calls and their outcomes — what was \
                        created, modified, or deleted\n\
                     4. **Current State**: Where things stand now\n\
                     5. **Pending Work**: Unfinished tasks or planned next steps\n\n\
                     CRITICAL: In section 2 (Key Data), copy values character-for-character. \
                     Do NOT paraphrase cron expressions, file paths, or IDs.\n\n\
                     ---\n\n{history}"
                )),
            }],
            tools: vec![], // no tools — compact must only produce text
            system: Some(
                "You are a conversation summarizer. Produce a dense, accurate, \
                 structured summary. NEVER call tools. Text output only.".to_owned(),
            ),
            max_tokens: Some(4096), // structured output needs more room
            temperature: None,
            frequency_penalty: None,
            thinking_budget: None,
        };

        let providers = Arc::clone(&self.providers);
        let mut stream = match self.failover.call(req, &providers).await {
            Ok(s) => s,
            Err(e) => {
                warn!("compaction LLM call failed: {e:#}");
                return None;
            }
        };

        let mut summary = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(d)) => summary.push_str(&d),
                Ok(StreamEvent::ReasoningDelta(_)) => {} // ignore reasoning in compaction
                Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
                Ok(StreamEvent::ToolCall { .. }) => {} // unexpected in summarization
                Err(e) => {
                    warn!("compaction stream error: {e:#}");
                    return None;
                }
            }
        }

        if summary.is_empty() {
            None
        } else {
            Some(summary)
        }
    }

    /// Extract key facts (names, IDs, decisions, file paths) from a
    /// conversation transcript for long-term memory storage.
    pub(crate) async fn extract_key_facts(&mut self, model: &str, history: &str) -> Option<String> {
        // Limit input to avoid huge summarisation calls.
        let input = if history.len() > 60_000 {
            let mut end = 60_000;
            while end < history.len() && !history.is_char_boundary(end) {
                end += 1;
            }
            &history[..end]
        } else {
            history
        };
        let req = LlmRequest {
            model: model.to_owned(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(format!(
                    "Extract the key facts from this conversation that should be remembered \
                     long-term. Output ONLY a bullet list (one fact per line, prefixed with \
                     '- '). Include: names, user IDs, chat IDs, phone numbers, account numbers, \
                     any numeric sequences that were looked up or confirmed, important decisions, \
                     file paths, URLs, preferences, and action items. \
                     IMPORTANT: copy numeric values (phone numbers, IDs) character-for-character — \
                     never truncate or paraphrase them. Be concise. Skip ephemeral chit-chat.\n\n{input}"
                )),
            }],
            tools: vec![],
            system: Some(
                "You extract key facts from conversations. Output only a bullet list.".to_owned(),
            ),
            max_tokens: Some(1024),
            temperature: None,
            frequency_penalty: None,
            thinking_budget: None,
        };

        let providers = Arc::clone(&self.providers);
        let mut stream = match self.failover.call(req, &providers).await {
            Ok(s) => s,
            Err(e) => {
                warn!("key fact extraction failed: {e:#}");
                return None;
            }
        };

        let mut result = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(d)) => result.push_str(&d),
                Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
                _ => {}
            }
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    // -----------------------------------------------------------------------
    // JSONL transcript (AGENTS.md $20 step 11)
    // -----------------------------------------------------------------------

    /// Append user + assistant messages to `~/.rsclaw/transcripts/<key>.jsonl`.
    pub(crate) async fn append_transcript(&self, session_key: &str, user_text: &str, assistant_text: &str) {
        let transcripts_dir = dirs_next::home_dir()
            .unwrap_or_default()
            .join(".rsclaw/transcripts");

        // Sanitize session key for use as a filename.
        let safe_key: String = session_key
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = transcripts_dir.join(format!("{safe_key}.jsonl"));

        if let Err(e) = tokio::fs::create_dir_all(&transcripts_dir).await {
            warn!("transcript mkdir: {e:#}");
            return;
        }

        let ts = Utc::now().to_rfc3339();
        let mut lines = String::new();
        for (role, content) in [("user", user_text), ("assistant", assistant_text)] {
            let entry = json!({
                "role": role,
                "content": content,
                "session": session_key,
                "agent": self.handle.id,
                "ts": ts,
            });
            if let Ok(s) = serde_json::to_string(&entry) {
                lines.push_str(&s);
                lines.push('\n');
            }
        }

        match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(lines.as_bytes()).await {
                    warn!("transcript write: {e:#}");
                }
            }
            Err(e) => warn!("transcript open: {e:#}"),
        }
    }
}
