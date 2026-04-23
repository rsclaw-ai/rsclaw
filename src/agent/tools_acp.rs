//! ACP (Agent Client Protocol) tool handlers for OpenCode and Claude Code.
//!
//! Extracted from `runtime.rs` to keep the main agent loop file focused.
//! These methods live in a separate `impl AgentRuntime` block which Rust
//! allows across multiple files within the same crate.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::registry::{AgentMessage, AgentReply};
use super::runtime::{AgentRuntime, RunContext, expand_tilde};

impl AgentRuntime {
    /// Get or create the OpenCode ACP client.
    /// Uses the agent's workspace directory as cwd for proper file
    /// organization.
    pub(crate) async fn get_opencode_client(&self) -> Result<crate::acp::client::AcpClient> {
        if let Some(client) = self.opencode_client.get() {
            return Ok(client.clone());
        }

        // Find opencode executable
        let command = which::which("opencode")
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|_| std::env::var("OPENCODE_PATH"))
            .unwrap_or_else(|_| "opencode".to_string());

        // Use "acp" subcommand to start ACP protocol mode
        let args: Vec<&str> = vec!["acp"];

        tracing::info!(command = %command, args = ?args, "OpenCode: starting subprocess");

        // Use agent's workspace directory instead of current_dir
        // This ensures files are created in the right location
        let cwd = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"))
            .to_string_lossy()
            .to_string();

        tracing::info!(cwd = %cwd, "OpenCode: using workspace directory");

        let client = crate::acp::client::AcpClient::spawn(&command, &args).await?;
        client
            .initialize("rsclaw", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"))
            .await?;

        // Create session with model from config or environment
        let model = self
            .handle
            .config
            .opencode
            .as_ref()
            .and_then(|c| c.model.clone())
            .or_else(|| std::env::var("OPENCODE_MODEL").ok());
        let session_resp = client.create_session(&cwd, model.as_deref(), None).await?;

        tracing::info!(
            session_id = %session_resp.session_id,
            current_model = ?session_resp.models.as_ref().and_then(|m| m.available_models.first()).map(|m| &m.model_id),
            "OpenCode session created"
        );

        self.opencode_client.set(client.clone()).ok();
        Ok(client)
    }

    /// Tool handler for opencode ACP calls - runs asynchronously.
    /// Results are delivered via notification channel when complete.
    pub(crate) async fn tool_opencode(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| anyhow!("opencode tool requires 'task' argument"))?;

        tracing::info!(task = %task, "tool_opencode: starting");

        let lang = self.config.raw.gateway.as_ref()
            .and_then(|g| g.language.as_deref())
            .map(crate::i18n::resolve_lang)
            .unwrap_or("en");

        // Get notification sender early for error reporting
        let notif_tx = self.notification_tx.clone();
        let target_id = ctx.peer_id.clone();
        let channel_name = ctx.channel.clone();

        // Try to get client, send error notification if failed
        let client = match self.get_opencode_client().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("tool_opencode: get_client failed: {}", e);
                if let Some(ref tx) = notif_tx {
                    let _ = tx.send(crate::channel::OutboundMessage {
                        target_id: target_id.clone(),
                        is_group: false,
                        text: crate::i18n::t_fmt("acp_start_failed", lang, &[("name", "OpenCode"), ("error", &e.to_string())]),
                        reply_to: None,
                        images: vec![],
                        files: vec![],
                        channel: Some(channel_name.clone()),
                    });
                }
                return Err(e);
            }
        };
        let session_id = client.session_id().await.unwrap_or_default();
        let session_id_clone = session_id.clone();

        let task_str = task.to_string();

        // Send initial notification
        tracing::info!("tool_opencode: sending initial notification to {}", target_id);
        if let Some(ref tx) = notif_tx {
            let _ = tx.send(crate::channel::OutboundMessage {
                target_id: target_id.clone(),
                is_group: false,
                text: crate::i18n::t_fmt("acp_submitted", lang, &[("name", "OpenCode")]),
                reply_to: None,
                images: vec![],
                files: vec![],
                channel: Some(channel_name.clone()),
            });
        } else {
            tracing::warn!("tool_opencode: no notification_tx for initial notification");
        }

        // Spawn background task - collect events AND send prompt in parallel
        let notif_tx_bg = notif_tx.clone();
        let target_id_bg = target_id.clone();
        let channel_bg = channel_name.clone();
        let lang_bg = lang;
        // Clone agent's own inbox for result injection after completion.
        let self_tx = self.handle.tx.clone();
        let self_session = ctx.session_key.clone();
        let self_channel = ctx.channel.clone();
        let self_peer_id = ctx.peer_id.clone();
        let self_chat_id = ctx.chat_id.clone();
        tokio::spawn(async move {
            tracing::info!("tool_opencode: background task started");
            // Start event collection FIRST (in parallel with send_prompt)
            let mut event_rx = client.subscribe_events();
            let events = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
            let events_clone = Arc::clone(&events);

            // Event collection task - collects events for final summary, NO intermediate notifications
            let _event_collector = tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            // Only collect tool call events for summary, skip thoughts (AgentThoughtChunk)
                            let event_str = match &event {
                                crate::acp::client::SessionEvent::ToolCallStarted {
                                    title, ..
                                } => {
                                    let s = format!("🔧 {}", title.as_deref().unwrap_or("tool"));
                                    tracing::info!("OpenCode event: {}", s);
                                    s
                                }
                                crate::acp::client::SessionEvent::ToolCallCompleted {
                                    result,
                                    ..
                                } => {
                                    let s = result
                                        .as_ref()
                                        .map(|r| {
                                            if r.chars().count() > 100 {
                                                let cutoff = r
                                                    .char_indices()
                                                    .nth(100)
                                                    .map(|(i, _)| i)
                                                    .unwrap_or(r.len());
                                                format!("✅ {}...", &r[..cutoff])
                                            } else {
                                                format!("✅ {}", r)
                                            }
                                        })
                                        .unwrap_or_default();
                                    tracing::info!("OpenCode event: {}", s);
                                    s
                                }
                                crate::acp::client::SessionEvent::ToolCallFailed {
                                    error, ..
                                } => {
                                    let s = format!("❌ {}", error);
                                    tracing::info!("OpenCode event: {}", s);
                                    s
                                }
                                crate::acp::client::SessionEvent::AgentMessageChunk {
                                    content,
                                } => {
                                    // Log message chunks for visibility
                                    tracing::debug!("OpenCode message: {}", content);
                                    String::new()
                                }
                                // Skip AgentThoughtChunk - don't send thinking messages to user
                                _ => String::new(),
                            };

                            if !event_str.is_empty() {
                                events_clone.lock().await.push(event_str.clone());
                            }
                        }
                        Err(_) => break,
                    }
                }
            });

            // Send prompt (runs in parallel with event collection)
            tracing::info!("tool_opencode: sending prompt");
            let send_result = client.send_prompt(&task_str).await;

            // DON'T wait for event collector - it runs forever! Just get what we have so
            // far. The events collected during execution are already in
            // `events`.

            // Process the result — collect summary + files for both notification and agent re-inject.
            let mut result_summary = String::new();
            let mut result_files: Vec<(String, String, String)> = vec![];
            match send_result {
                Ok(resp) => {
                    tracing::info!(
                        "tool_opencode: send_prompt completed, stop_reason={:?}",
                        resp.stop_reason
                    );

                    let events_list = events.lock().await.clone();
                    let collected = client.get_collected_content().await;
                    tracing::info!(
                        "tool_opencode: events count={}, collected len={}",
                        events_list.len(),
                        collected.len()
                    );

                    // Get the final result content
                    let result_content = if !collected.is_empty() {
                        collected
                    } else if let Some(result) = resp.result {
                        result
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                crate::acp::types::ContentBlock::Text { text } => {
                                    Some(text.clone())
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        String::new()
                    };

                    // Build a concise summary instead of dumping all events
                    let tool_count = events_list.iter().filter(|e| e.starts_with("🔧")).count();
                    let status_icon = match resp.stop_reason {
                        crate::acp::types::StopReason::EndTurn => "✅",
                        crate::acp::types::StopReason::MaxTokens => "⚠️",
                        crate::acp::types::StopReason::Cancelled => "⏹️",
                        crate::acp::types::StopReason::Incomplete => "❓",
                    };

                    // Scan result_content for downloadable file paths (e.g. mp4 downloaded by opencode).
                    let notif_files: Vec<(String, String, String)> = {
                        let sendable_exts = [".mp4", ".mp3", ".zip", ".pdf", ".xlsx", ".docx", ".pptx", ".csv", ".tar.gz"];
                        let mut found = Vec::new();
                        for token in result_content.split_whitespace() {
                            let trimmed = token.trim_matches(|c: char| "\"'.,;:()[]{}".contains(c));
                            // Strip any leading non-path characters (e.g. Chinese prefix like "路径：")
                            // by finding the first '/' or '~' in the token.
                            let trimmed = if let Some(pos) = trimmed.find(|c| c == '/' || c == '~') {
                                &trimmed[pos..]
                            } else {
                                trimmed
                            };
                            let lower = trimmed.to_lowercase();
                            if sendable_exts.iter().any(|ext| lower.ends_with(ext)) {
                                let path = expand_tilde(trimmed);
                                if path.exists() {
                                    if let Ok(meta) = path.metadata() {
                                        if meta.len() <= 50_000_000 {
                                            let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                                            let mime = if lower.ends_with(".mp4") { "video/mp4" }
                                                else if lower.ends_with(".mp3") { "audio/mpeg" }
                                                else if lower.ends_with(".pdf") { "application/pdf" }
                                                else if lower.ends_with(".zip") || lower.ends_with(".tar.gz") { "application/zip" }
                                                else if lower.ends_with(".xlsx") { "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" }
                                                else if lower.ends_with(".docx") { "application/vnd.openxmlformats-officedocument.wordprocessingml.document" }
                                                else if lower.ends_with(".pptx") { "application/vnd.openxmlformats-officedocument.presentationml.presentation" }
                                                else { "text/csv" };
                                            let path_str = path.to_string_lossy().to_string();
                                            if !found.iter().any(|(_, _, p): &(String, String, String)| p == &path_str) {
                                                tracing::info!(path = %path_str, "tool_opencode: attaching file to notification");
                                                found.push((filename, mime.to_owned(), path_str));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        found
                    };

                    let summary = if !result_content.is_empty() {
                        // Show result, truncated if too long (character-safe truncation)
                        let truncated = if result_content.chars().count() > 2000 {
                            let cutoff = result_content
                                .char_indices()
                                .nth(2000)
                                .map(|(i, _)| i)
                                .unwrap_or(result_content.len());
                            crate::i18n::t_fmt("acp_truncated", lang_bg, &[("content", &result_content[..cutoff])])
                        } else {
                            result_content
                        };
                        crate::i18n::t_fmt("acp_done_result", lang_bg, &[
                            ("status", status_icon), ("name", "OpenCode"),
                            ("count", &tool_count.to_string()), ("result", &truncated),
                        ])
                    } else if tool_count > 0 {
                        crate::i18n::t_fmt("acp_done_summary", lang_bg, &[
                            ("status", status_icon), ("name", "OpenCode"),
                            ("count", &tool_count.to_string()), ("summary", &events_list.join("\n")),
                        ])
                    } else {
                        crate::i18n::t_fmt("acp_done_empty", lang_bg, &[
                            ("status", status_icon), ("name", "OpenCode"),
                        ])
                    };

                    // Store for agent re-inject after notification.
                    result_summary = summary.clone();
                    result_files = notif_files.clone();

                    // Send notification to user
                    tracing::info!(
                        summary_preview = %summary.chars().take(100).collect::<String>(),
                        files_count = notif_files.len(),
                        target = %target_id_bg,
                        channel = %channel_bg,
                        "tool_opencode: sending completion notification"
                    );
                    if let Some(ref tx) = notif_tx_bg {
                        match tx.send(crate::channel::OutboundMessage {
                            target_id: target_id_bg.clone(),
                            is_group: false,
                            text: summary,
                            reply_to: None,
                            images: vec![],
                            files: notif_files,
                            channel: Some(channel_bg.clone()),
                        }) {
                            Ok(_) => {
                                tracing::info!("tool_opencode: notification sent successfully to {}", target_id_bg);
                            }
                            Err(e) => {
                                tracing::error!("tool_opencode: failed to send notification: {}", e);
                            }
                        }
                    } else {
                        tracing::warn!("tool_opencode: no notification channel available");
                    }
                }
                Err(e) => {
                    tracing::error!("tool_opencode: send_prompt failed: {}", e);
                    if let Some(ref tx) = notif_tx_bg {
                        tracing::info!("tool_opencode: sending error notification to {}", target_id_bg);
                        let _ = tx.send(crate::channel::OutboundMessage {
                            target_id: target_id_bg.clone(),
                            is_group: false,
                            text: crate::i18n::t_fmt("acp_error", lang_bg, &[("name", "OpenCode"), ("error", &e.to_string())]),
                            reply_to: None,
                            images: vec![],
                            files: vec![],
                            channel: Some(channel_bg.clone()),
                        });
                    }
                }
            }
            tracing::info!("tool_opencode: background task finished");
            // IMPORTANT: DON'T await event_collector - it runs forever waiting
            // for more events The collected events are already in
            // `events` variable

            // Inject result back into main agent's inbox so it can act on the result
            // (e.g. send_file). This triggers a new agent turn.
            let file_paths: Vec<String> = result_files.iter().map(|(_, _, p)| p.clone()).collect();
            let inject_text = if file_paths.is_empty() {
                format!("[OpenCode completed] {}", if result_summary.is_empty() { "Task finished.".to_owned() } else { result_summary })
            } else {
                format!("[OpenCode completed] Files ready: {}. Please send them to the user with send_file.",
                    file_paths.join(", "))
            };
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentReply>();
            let inject_msg = AgentMessage {
                session_key: self_session,
                text: inject_text,
                channel: self_channel.clone(),
                peer_id: self_peer_id,
                chat_id: self_chat_id,
                reply_tx,
                extra_tools: vec![],
                images: vec![],
                files: vec![],
            };
            if self_tx.send(inject_msg).await.is_err() {
                tracing::warn!("tool_opencode: failed to inject result back to agent inbox");
            } else {
                tracing::info!("tool_opencode: result injected back to agent, waiting for reply");
                // Wait for agent's reply and forward it (text + files) to user via notification.
                match tokio::time::timeout(Duration::from_secs(300), reply_rx).await {
                    Ok(Ok(reply)) => {
                        if !reply.text.is_empty() || !reply.files.is_empty() || !reply.images.is_empty() {
                            if let Some(ref tx) = notif_tx_bg {
                                let _ = tx.send(crate::channel::OutboundMessage {
                                    target_id: target_id_bg.clone(),
                                    is_group: false,
                                    text: reply.text,
                                    reply_to: None,
                                    images: reply.images,
                                    files: reply.files,
                                    channel: Some(self_channel),
                                });
                                tracing::info!("tool_opencode: forwarded agent reply to user");
                            }
                        }
                    }
                    Ok(Err(_)) => tracing::warn!("tool_opencode: reply channel dropped"),
                    Err(_) => tracing::warn!("tool_opencode: reply timed out after 300s"),
                }
            }
        });

        Ok(serde_json::json!({
            "output": crate::i18n::t_fmt("acp_queued", lang, &[("name", "OpenCode")]),
            "status": "submitted",
            "session_id": session_id_clone
        }))
    }

    // -----------------------------------------------------------------------
    // Claude Code ACP integration
    // -----------------------------------------------------------------------

    /// Get or create the Claude Code ACP client.
    /// Uses claude-agent-acp which wraps Claude Agent SDK with ACP protocol.
    pub(crate) async fn get_claudecode_client(&self) -> Result<crate::acp::client::AcpClient> {
        if let Some(client) = self.claudecode_client.get() {
            return Ok(client.clone());
        }

        // Find claude-agent-acp executable
        // Can be installed via npm: npm install -g @agentclientprotocol/claude-agent-acp
        let (command, args) = if let Ok(path) = which::which("claude-agent-acp") {
            (path.to_string_lossy().to_string(), vec![])
        } else if let Ok(path) = std::env::var("CLAUDE_AGENT_ACP_PATH") {
            // If it's a .js file, run with node
            if path.ends_with(".js") {
                ("node".to_string(), vec![path])
            } else {
                (path, vec![])
            }
        } else {
            // Try common npm global install paths
            let npm_global = std::env::var("npm_config_prefix").ok();
            let js_path = npm_global
                .as_ref()
                .map(|p| {
                    let mut path = std::path::PathBuf::from(p);
                    path.push("node_modules");
                    path.push("@agentclientprotocol");
                    path.push("claude-agent-acp");
                    path.push("dist");
                    path.push("index.js");
                    path
                })
                .or_else(|| {
                    dirs_next::home_dir().map(|h| {
                        let mut path = h;
                        path.push(".npm-global");
                        path.push("node_modules");
                        path.push("@agentclientprotocol");
                        path.push("claude-agent-acp");
                        path.push("dist");
                        path.push("index.js");
                        path
                    })
                })
                .or_else(|| {
                    dirs_next::home_dir().map(|h| {
                        let mut path = h;
                        path.push("node_modules");
                        path.push("@agentclientprotocol");
                        path.push("claude-agent-acp");
                        path.push("dist");
                        path.push("index.js");
                        path
                    })
                });

            match js_path {
                Some(p) if p.exists() => {
                    // .js files need to be run with node
                    ("node".to_string(), vec![p.to_string_lossy().to_string()])
                }
                _ => {
                    // Fallback - let spawn handle the error
                    ("claude-agent-acp".to_string(), vec![])
                }
            }
        };

        tracing::info!(command = %command, "Claude Code: starting subprocess");

        // Use agent's workspace directory
        let cwd = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"))
            .to_string_lossy()
            .to_string();

        tracing::info!(cwd = %cwd, args = ?args, "Claude Code: using workspace directory");

        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let client = crate::acp::client::AcpClient::spawn(&command, &args_ref).await?;
        client
            .initialize("rsclaw", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"))
            .await?;

        // Create session with model if configured
        let model = self
            .handle
            .config
            .claudecode
            .as_ref()
            .and_then(|c| c.model.clone())
            .or_else(|| std::env::var("CLAUDE_MODEL").ok())
            .or_else(|| std::env::var("ANTHROPIC_MODEL").ok());
        eprintln!("[ClaudeCode] model resolution: model={:?}, claudecode_config={:?}", model, self.handle.config.claudecode);
        let session_resp = client.create_session(&cwd, model.as_deref(), None).await?;

        tracing::info!(
            session_id = %session_resp.session_id,
            "Claude Code session created"
        );

        // Set model explicitly after session creation (modelId in session/new doesn't switch model)
        if let Some(ref m) = model {
            tracing::info!(model = %m, "Claude Code: setting model after session creation");
            client.set_model(m).await?;
        }

        self.claudecode_client.set(client.clone()).ok();
        Ok(client)
    }

    /// Tool handler for Claude Code ACP calls - runs asynchronously.
    /// Results are delivered via notification channel when complete.
    pub(crate) async fn tool_claudecode(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| anyhow!("claudecode tool requires 'task' argument"))?;

        tracing::info!(task = %task, "tool_claudecode: starting");

        let lang = self.config.raw.gateway.as_ref()
            .and_then(|g| g.language.as_deref())
            .map(crate::i18n::resolve_lang)
            .unwrap_or("en");

        // Get notification sender early for error reporting
        let notif_tx = self.notification_tx.clone();
        let target_id = ctx.peer_id.clone();
        let channel_name = ctx.channel.clone();

        // Try to get client, send error notification if failed
        let client = match self.get_claudecode_client().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("tool_claudecode: get_client failed: {}", e);
                if let Some(ref tx) = notif_tx {
                    let _ = tx.send(crate::channel::OutboundMessage {
                        target_id: target_id.clone(),
                        is_group: false,
                        text: crate::i18n::t_fmt("acp_start_failed", lang, &[("name", "Claude Code"), ("error", &e.to_string())]),
                        reply_to: None,
                        images: vec![],
                        files: vec![],
                        channel: Some(channel_name.clone()),
                    });
                }
                return Err(e);
            }
        };
        let session_id = client.session_id().await.unwrap_or_default();
        let session_id_clone = session_id.clone();

        let task_str = task.to_string();

        // Send initial notification
        if let Some(ref tx) = notif_tx {
            let _ = tx.send(crate::channel::OutboundMessage {
                target_id: target_id.clone(),
                is_group: false,
                text: crate::i18n::t_fmt("acp_submitted", lang, &[("name", "Claude Code")]),
                reply_to: None,
                images: vec![],
                files: vec![],
                channel: Some(channel_name.clone()),
            });
        }

        // Spawn background task - collect events AND send prompt in parallel
        let notif_tx_bg = notif_tx.clone();
        let target_id_bg = target_id.clone();
        let channel_bg = channel_name.clone();
        let lang_bg = lang;
        tokio::spawn(async move {
            // Start event collection FIRST (in parallel with send_prompt)
            let mut event_rx = client.subscribe_events();
            let events = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
            let events_clone = Arc::clone(&events);

            // Event collection task - collects events for final summary, NO intermediate notifications
            let _event_collector = tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            // Only collect tool call events for summary, skip thoughts (AgentThoughtChunk)
                            let event_str = match &event {
                                crate::acp::client::SessionEvent::ToolCallStarted {
                                    title, ..
                                } => {
                                    let s = format!("🔧 {}", title.as_deref().unwrap_or("tool"));
                                    tracing::info!("OpenCode event: {}", s);
                                    s
                                }
                                crate::acp::client::SessionEvent::ToolCallCompleted {
                                    result,
                                    ..
                                } => {
                                    let s = result
                                        .as_ref()
                                        .map(|r| {
                                            if r.chars().count() > 100 {
                                                let cutoff = r
                                                    .char_indices()
                                                    .nth(100)
                                                    .map(|(i, _)| i)
                                                    .unwrap_or(r.len());
                                                format!("✅ {}...", &r[..cutoff])
                                            } else {
                                                format!("✅ {}", r)
                                            }
                                        })
                                        .unwrap_or_default();
                                    tracing::info!("OpenCode event: {}", s);
                                    s
                                }
                                crate::acp::client::SessionEvent::ToolCallFailed {
                                    error, ..
                                } => {
                                    let s = format!("❌ {}", error);
                                    tracing::info!("OpenCode event: {}", s);
                                    s
                                }
                                crate::acp::client::SessionEvent::AgentMessageChunk {
                                    content,
                                } => {
                                    // Log message chunks for visibility
                                    tracing::debug!("OpenCode message: {}", content);
                                    String::new()
                                }
                                // Skip AgentThoughtChunk - don't send thinking messages to user
                                _ => String::new(),
                            };

                            if !event_str.is_empty() {
                                events_clone.lock().await.push(event_str.clone());
                            }
                        }
                        Err(_) => break,
                    }
                }
            });

            // Send prompt (runs in parallel with event collection)
            tracing::info!("tool_claudecode: sending prompt");
            let send_result = client.send_prompt(&task_str).await;

            // Process the result
            match send_result {
                Ok(resp) => {
                    tracing::info!(
                        "tool_claudecode: send_prompt completed, stop_reason={:?}",
                        resp.stop_reason
                    );

                    let events_list = events.lock().await.clone();
                    let collected = client.get_collected_content().await;
                    tracing::info!(
                        "tool_claudecode: events count={}, collected len={}",
                        events_list.len(),
                        collected.len()
                    );

                    // Get the final result content
                    let result_content = if !collected.is_empty() {
                        collected
                    } else if let Some(result) = resp.result {
                        result
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                crate::acp::types::ContentBlock::Text { text } => {
                                    Some(text.clone())
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        String::new()
                    };

                    // Build a concise summary instead of dumping all events
                    let tool_count = events_list.iter().filter(|e| e.starts_with("🔧")).count();
                    let status_icon = match resp.stop_reason {
                        crate::acp::types::StopReason::EndTurn => "✅",
                        crate::acp::types::StopReason::MaxTokens => "⚠️",
                        crate::acp::types::StopReason::Cancelled => "⏹️",
                        crate::acp::types::StopReason::Incomplete => "❓",
                    };

                    // Scan result_content for downloadable file paths.
                    let notif_files: Vec<(String, String, String)> = {
                        let sendable_exts = [".mp4", ".mp3", ".zip", ".pdf", ".xlsx", ".docx", ".pptx", ".csv", ".tar.gz"];
                        let mut found = Vec::new();
                        for token in result_content.split_whitespace() {
                            let trimmed = token.trim_matches(|c: char| "\"'.,;:()[]{}".contains(c));
                            // Strip any leading non-path characters (e.g. Chinese prefix like "路径：")
                            // by finding the first '/' or '~' in the token.
                            let trimmed = if let Some(pos) = trimmed.find(|c| c == '/' || c == '~') {
                                &trimmed[pos..]
                            } else {
                                trimmed
                            };
                            let lower = trimmed.to_lowercase();
                            if sendable_exts.iter().any(|ext| lower.ends_with(ext)) {
                                let path = expand_tilde(trimmed);
                                if path.exists() {
                                    if let Ok(meta) = path.metadata() {
                                        if meta.len() <= 50_000_000 {
                                            let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                                            let mime = if lower.ends_with(".mp4") { "video/mp4" }
                                                else if lower.ends_with(".mp3") { "audio/mpeg" }
                                                else if lower.ends_with(".pdf") { "application/pdf" }
                                                else if lower.ends_with(".zip") || lower.ends_with(".tar.gz") { "application/zip" }
                                                else if lower.ends_with(".xlsx") { "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" }
                                                else if lower.ends_with(".docx") { "application/vnd.openxmlformats-officedocument.wordprocessingml.document" }
                                                else if lower.ends_with(".pptx") { "application/vnd.openxmlformats-officedocument.presentationml.presentation" }
                                                else { "text/csv" };
                                            let path_str = path.to_string_lossy().to_string();
                                            if !found.iter().any(|(_, _, p): &(String, String, String)| p == &path_str) {
                                                tracing::info!(path = %path_str, "tool_claudecode: attaching file to notification");
                                                found.push((filename, mime.to_owned(), path_str));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        found
                    };

                    let summary = if !result_content.is_empty() {
                        // Show result, truncated if too long (character-safe truncation)
                        let truncated = if result_content.chars().count() > 2000 {
                            let cutoff = result_content
                                .char_indices()
                                .nth(2000)
                                .map(|(i, _)| i)
                                .unwrap_or(result_content.len());
                            crate::i18n::t_fmt("acp_truncated", lang_bg, &[("content", &result_content[..cutoff])])
                        } else {
                            result_content
                        };
                        crate::i18n::t_fmt("acp_done_result", lang_bg, &[
                            ("status", status_icon), ("name", "Claude Code"),
                            ("count", &tool_count.to_string()), ("result", &truncated),
                        ])
                    } else if tool_count > 0 {
                        crate::i18n::t_fmt("acp_done_summary", lang_bg, &[
                            ("status", status_icon), ("name", "Claude Code"),
                            ("count", &tool_count.to_string()), ("summary", &events_list.join("\n")),
                        ])
                    } else {
                        crate::i18n::t_fmt("acp_done_empty", lang_bg, &[
                            ("status", status_icon), ("name", "Claude Code"),
                        ])
                    };

                    // Send notification to user
                    tracing::debug!(
                        "tool_claudecode: sending completion notification, summary_len={}, files={}",
                        summary.len(), notif_files.len()
                    );
                    if let Some(ref tx) = notif_tx_bg {
                        match tx.send(crate::channel::OutboundMessage {
                            target_id: target_id_bg.clone(),
                            is_group: false,
                            text: summary,
                            reply_to: None,
                            images: vec![],
                            files: notif_files,
                            channel: Some(channel_bg.clone()),
                        }) {
                            Ok(_) => {
                                tracing::debug!("tool_claudecode: notification sent successfully")
                            }
                            Err(e) => {
                                tracing::error!("tool_claudecode: failed to send notification: {}", e)
                            }
                        }
                    } else {
                        tracing::warn!("tool_claudecode: no notification channel available");
                    }
                }
                Err(e) => {
                    tracing::error!("tool_claudecode: send_prompt failed: {}", e);
                    if let Some(ref tx) = notif_tx_bg {
                        let _ = tx.send(crate::channel::OutboundMessage {
                            target_id: target_id_bg.clone(),
                            is_group: false,
                            text: crate::i18n::t_fmt("acp_error", lang_bg, &[("name", "Claude Code"), ("error", &e.to_string())]),
                            reply_to: None,
                            images: vec![],
                            files: vec![],
                            channel: Some(channel_bg.clone()),
                        });
                    }
                }
            }
            // DON'T await event_collector - it runs forever
        });

        Ok(serde_json::json!({
            "output": crate::i18n::t_fmt("acp_queued", lang, &[("name", "Claude Code")]),
            "status": "submitted",
            "session_id": session_id_clone
        }))
    }

    // -----------------------------------------------------------------------
    // Codex MCP integration
    // -----------------------------------------------------------------------

    /// Get or create the Codex MCP client.
    /// Uses Codex CLI's MCP server mode (codex mcp-server).
    pub(crate) async fn get_codex_client(&self) -> Result<crate::acp::CodexClient> {
        if let Some(client) = self.codex_client.get() {
            return Ok(client.clone());
        }

        // Find codex executable
        let command = self
            .handle
            .config
            .codex
            .as_ref()
            .and_then(|c| c.command.clone())
            .or_else(|| which::which("codex").map(|p| p.to_string_lossy().to_string()).ok())
            .or_else(|| std::env::var("CODEX_PATH").ok())
            .unwrap_or_else(|| "codex".to_string());

        // Use agent's workspace directory
        let cwd = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

        // Get model from config or environment
        let model = self
            .handle
            .config
            .codex
            .as_ref()
            .and_then(|c| c.model.clone())
            .or_else(|| std::env::var("CODEX_MODEL").ok());

        tracing::info!(command = %command, cwd = %cwd.display(), model = ?model, "Codex: starting MCP server");

        let client = crate::acp::CodexClient::spawn(cwd, Some(&command), model.as_deref()).await?;

        self.codex_client.set(client.clone()).ok();
        Ok(client)
    }

    /// Tool handler for Codex MCP calls - runs asynchronously.
    /// Results are delivered via notification channel when complete.
    /// Codex uses MCP protocol (not ACP), so it's simpler - no event streaming.
    pub(crate) async fn tool_codex(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| anyhow!("codex tool requires 'task' argument"))?;

        tracing::info!(task = %task, "tool_codex: starting");

        let lang = self.config.raw.gateway.as_ref()
            .and_then(|g| g.language.as_deref())
            .map(crate::i18n::resolve_lang)
            .unwrap_or("en");

        // Get notification sender early for error reporting
        let notif_tx = self.notification_tx.clone();
        let target_id = ctx.peer_id.clone();
        let channel_name = ctx.channel.clone();

        // Try to get client, send error notification if failed
        let client = match self.get_codex_client().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("tool_codex: get_client failed: {}", e);
                if let Some(ref tx) = notif_tx {
                    let _ = tx.send(crate::channel::OutboundMessage {
                        target_id: target_id.clone(),
                        is_group: false,
                        text: crate::i18n::t_fmt("acp_start_failed", lang, &[("name", "Codex"), ("error", &e.to_string())]),
                        reply_to: None,
                        images: vec![],
                        files: vec![],
                        channel: Some(channel_name.clone()),
                    });
                }
                return Err(e);
            }
        };

        let task_str = task.to_string();

        // Send initial notification
        if let Some(ref tx) = notif_tx {
            let _ = tx.send(crate::channel::OutboundMessage {
                target_id: target_id.clone(),
                is_group: false,
                text: crate::i18n::t_fmt("acp_submitted", lang, &[("name", "Codex")]),
                reply_to: None,
                images: vec![],
                files: vec![],
                channel: Some(channel_name.clone()),
            });
        }

        // Spawn background task
        let notif_tx_bg = notif_tx.clone();
        let target_id_bg = target_id.clone();
        let channel_bg = channel_name.clone();
        let lang_bg = lang;
        tokio::spawn(async move {
            tracing::info!("tool_codex: background task started, calling execute");

            let result = client.execute(&task_str).await;

            match result {
                Ok(codex_result) => {
                    tracing::info!(
                        thread_id = ?codex_result.thread_id,
                        content_len = codex_result.content.len(),
                        "tool_codex: execute completed"
                    );

                    // Build summary
                    let content = codex_result.content;
                    let truncated = if content.chars().count() > 2000 {
                        let cutoff = content
                            .char_indices()
                            .nth(2000)
                            .map(|(i, _)| i)
                            .unwrap_or(content.len());
                        crate::i18n::t_fmt("acp_truncated", lang_bg, &[("content", &content[..cutoff])])
                    } else {
                        content.clone()
                    };

                    let summary = if content.is_empty() {
                        crate::i18n::t_fmt("acp_done_empty", lang_bg, &[("status", "✅"), ("name", "Codex")])
                    } else {
                        crate::i18n::t_fmt("acp_done_result", lang_bg, &[
                            ("status", "✅"), ("name", "Codex"),
                            ("count", "0"), ("result", &truncated),
                        ])
                    };

                    // Send notification to user
                    if let Some(ref tx) = notif_tx_bg {
                        let _ = tx.send(crate::channel::OutboundMessage {
                            target_id: target_id_bg.clone(),
                            is_group: false,
                            text: summary,
                            reply_to: None,
                            images: vec![],
                            files: vec![],
                            channel: Some(channel_bg.clone()),
                        });
                    }
                }
                Err(e) => {
                    tracing::error!("tool_codex: execute failed: {}", e);
                    if let Some(ref tx) = notif_tx_bg {
                        let _ = tx.send(crate::channel::OutboundMessage {
                            target_id: target_id_bg.clone(),
                            is_group: false,
                            text: crate::i18n::t_fmt("acp_error", lang_bg, &[("name", "Codex"), ("error", &e.to_string())]),
                            reply_to: None,
                            images: vec![],
                            files: vec![],
                            channel: Some(channel_bg.clone()),
                        });
                    }
                }
            }
            tracing::info!("tool_codex: background task finished");
        });

        Ok(serde_json::json!({
            "output": crate::i18n::t_fmt("acp_queued", lang, &[("name", "Codex")]),
            "status": "submitted"
        }))
    }
}
