//! Agent loop — the core LLM ↔ tool execution cycle (AGENTS.md §20).
//!
//! The `AgentRuntime` struct holds all dependencies for one agent instance.
//! `run_turn()` drives a single conversation turn:
//!   1. Build system prompt (workspace context + skills)
//!   2. Apply contextPruning to in-memory tool_results
//!   3. LLM streaming call
//!   4. Tool dispatch loop (skill / A2A / built-in)
//!   5. Loop detection
//!   6. Reply shaping (NO_REPLY filter)
//!   7. Write JSONL transcript
//!   8. Compaction check
//!   9. Auto-Recall (inject relevant memories) + Auto-Capture (store user
//!      message)

use std::{sync::Arc, sync::atomic::{AtomicBool, Ordering}, time::Duration};

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::{
    io::AsyncWriteExt as _,
    sync::{Mutex, RwLock, broadcast},
    time,
};
use tracing::{debug, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// LiveStatus — shared agent status for /btw parallel queries
// ---------------------------------------------------------------------------

/// Shared live status of an agent, readable by /btw side-channel queries
/// without going through the agent inbox.
#[derive(Debug, Clone, Default)]
pub struct LiveStatus {
    /// Current state: "idle", "thinking", "tool_call", "streaming".
    pub state: String,
    /// Brief description of what the agent is doing.
    pub current_task: String,
    /// Recent tool calls in the current turn.
    pub tool_history: Vec<String>,
    /// First ~200 chars of the streaming text response.
    pub text_preview: String,
    /// When the current turn started.
    pub started_at: Option<std::time::Instant>,
    /// Session key for the current turn.
    pub session_key: String,
}

use super::{
    exec_pool::ExecPool,
    loop_detection::LoopDetector,
    memory::{MemoryDoc, MemoryStore},
    registry::{AgentHandle, AgentMessage, AgentRegistry, AgentReply},
    tool_call_repair::repair_tool_result_pairing,
    workspace::{
        DEFAULT_MAX_CHARS_PER_FILE, DEFAULT_TOTAL_MAX_CHARS, SessionType, WorkspaceContext,
    },
};
use crate::{
    config::{runtime::RuntimeConfig, schema::ContextPruningConfig},
    events::AgentEvent,
    plugin::PluginRegistry,
    provider::{
        ContentPart, LlmRequest, Message, MessageContent, Role, StreamEvent, ToolDef,
        failover::FailoverManager, registry::ProviderRegistry,
    },
    skill::{RunOptions, SkillRegistry, run_tool},
    store::Store,
};

/// Agent-level timeout for a single turn (seconds).
/// Reduced from OpenClaw's 48h default to 30min for better UX.
/// Can be overridden via `agents.defaults.timeout_seconds`.
const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
/// Max consecutive tool parse errors before aborting the turn.
/// Prevents infinite retry loops when model output gets corrupted.
const MAX_PARSE_ERRORS: usize = 10;
/// Token string that suppresses any reply to the channel.
const NO_REPLY_TOKEN: &str = "NO_REPLY";
/// Default max file size before first confirmation (bytes): 50 MB.
const DEFAULT_MAX_FILE_SIZE: usize = 50_000_000;
/// Default max text chars before token confirmation.
const DEFAULT_MAX_TEXT_CHARS: usize = 50_000;

/// RAII guard that clears the abort flag for a session when dropped.
struct AbortFlagGuard {
    handle: Arc<AgentHandle>,
    session_key: String,
}

impl Drop for AbortFlagGuard {
    fn drop(&mut self) {
        // Always remove the entry — prevents leaking abort_flags entries for
        // sessions that complete normally (flag_value=false). Uses std::sync::RwLock
        // so .write() is safe in Drop (no .await needed).
        match self.handle.abort_flags.write() {
            Ok(mut flags) => {
                flags.remove(&self.session_key);
            }
            Err(e) => {
                tracing::warn!(
                    session = %self.session_key,
                    "AbortFlagGuard: failed to clean up abort flag: {e}"
                );
            }
        }
    }
}

/// Read-only commands that are always allowed for any agent (regardless of
/// allowedCommands).
const READONLY_COMMANDS: &[&str] = &[
    "/help", "/version", "/status", "/health", "/uptime", "/models", "/ctx", "/btw", "/clear",
    "/compact", "/history", "/cron", "/abort",
];

// ---------------------------------------------------------------------------
// PendingFile — file awaiting user confirmation (two-layer)
// ---------------------------------------------------------------------------

/// Processing stage for pending files.
enum PendingStage {
    /// Waiting for first confirmation (file too large).
    SizeConfirm,
    /// File processed, waiting for token confirmation.
    TokenConfirm {
        extracted_text: String,
        #[allow(dead_code)]
        estimated_tokens: usize,
    },
}

#[allow(dead_code)]
struct PendingFile {
    filename: String,
    path: std::path::PathBuf,
    size: usize,
    mime_type: String,
    /// Pre-encoded image data, if the file is an image.
    images: Vec<super::registry::ImageAttachment>,
    stage: PendingStage,
}

/// Check if the current model supports vision (image input).
fn model_supports_vision(model: &str, config: &RuntimeConfig) -> bool {
    // 1. Explicit config override
    if let Some(v) = config
        .ext
        .tools
        .as_ref()
        .and_then(|t| t.upload.as_ref())
        .and_then(|u| u.supports_vision)
    {
        return v;
    }

    // 2. Infer from model name
    let lower = model.to_lowercase();
    // Known vision models
    lower.contains("gpt-4o")
        || lower.contains("gpt-4-turbo")
        || lower.contains("gpt-4-vision")
        || lower.contains("claude-3")
        || lower.contains("claude-sonnet")
        || lower.contains("claude-opus")
        || lower.contains("claude-haiku")
        || lower.contains("gemini")
        || lower.contains("qwen-vl")
        || lower.contains("qwen2-vl")
        || lower.contains("glm-4v")
        || lower.contains("yi-vision")
        || lower.contains("internvl")
        || lower.contains("llava")
        || lower.contains("minicpm-v")
        || lower.contains("deepseek-vl")
        || lower.contains("qwen3")
        || lower.contains("doubao")
        || lower.contains("seed") // doubao-seed models
    // Known NON-vision models (deepseek-chat, deepseek-r1, qwen-turbo,
    // moonshot, minimax, etc.) return false by default.
}

// ---------------------------------------------------------------------------
// RunContext
// ---------------------------------------------------------------------------

/// Per-turn execution context.
pub struct RunContext {
    pub agent_id: String,
    pub session_key: String,
    pub channel: String,
    pub peer_id: String,
    /// Chat/conversation ID for sending intermediate progress messages.
    pub chat_id: String,
    /// Background exec pool for polling task results.
    pub exec_pool: Arc<ExecPool>,
    pub loop_detector: LoopDetector,
    /// Whether the current turn includes images.
    pub has_images: bool,
    /// The full user message with image data (for LLM, not persisted).
    pub user_msg_with_images: Option<Message>,
    /// Count of consecutive tool parse errors in this turn.
    pub parse_error_count: usize,
}

// ---------------------------------------------------------------------------
// AgentRuntime
// ---------------------------------------------------------------------------

pub struct AgentRuntime {
    pub handle: Arc<AgentHandle>,
    pub config: Arc<RuntimeConfig>,
    /// All registered providers — used by the failover manager.
    pub providers: Arc<ProviderRegistry>,
    /// Per-runtime failover manager tracking per-profile cooldowns.
    failover: FailoverManager,
    pub skills: Arc<SkillRegistry>,
    pub store: Arc<Store>,
    pub memory: Option<Arc<Mutex<MemoryStore>>>,
    pub agents: Option<Arc<AgentRegistry>>,
    /// SSE broadcast channel — None when running outside the gateway (e.g.
    /// tests).
    pub event_bus: Option<broadcast::Sender<AgentEvent>>,
    /// Dynamic agent spawner — None when running outside the gateway.
    pub spawner: Option<Arc<crate::agent::AgentSpawner>>,
    /// Plugin registry — None when running outside the gateway or with no
    /// plugins.
    pub plugins: Option<Arc<PluginRegistry>>,
    /// MCP server registry — None when no MCP servers are configured.
    pub mcp: Option<Arc<crate::mcp::McpRegistry>>,
    /// CDP browser session -- lazy-initialized on first web_browser tool call.
    /// Stored as Option so it can be dropped (killing Chrome) when idle expires.
    browser: Arc<tokio::sync::Mutex<Option<crate::browser::BrowserSession>>>,
    /// In-memory session cache: session_key -> conversation history.
    sessions: std::collections::HashMap<String, Vec<Message>>,
    /// Per-session compaction state: (last_compaction_time,
    /// turns_since_compaction).
    compaction_state: std::collections::HashMap<String, (std::time::Instant, u32)>,
    /// Pending large files awaiting user confirmation (session_key -> files).
    pending_files: std::collections::HashMap<String, Vec<PendingFile>>,
    /// Shared live status for /btw parallel queries.
    pub live_status: Arc<RwLock<LiveStatus>>,
    /// Runtime overrides (set by /set_upload_size, /set_upload_chars commands).
    runtime_max_file_size: Option<usize>,
    runtime_max_text_chars: Option<usize>,
    /// When this runtime was created.
    started_at: std::time::Instant,
    /// Cached workspace context (avoids re-reading unchanged files every turn).
    workspace_cache: Option<crate::agent::workspace::WorkspaceCache>,
    /// Background context manager (/ctx command, formerly /btw).
    btw_manager: super::btw::BtwManager,
    notification_tx: Option<tokio::sync::broadcast::Sender<crate::channel::OutboundMessage>>,
    opencode_client: Arc<tokio::sync::OnceCell<crate::acp::client::AcpClient>>,
    claudecode_client: Arc<tokio::sync::OnceCell<crate::acp::client::AcpClient>>,
    /// In-memory session alias cache: alias_key → canonical session_key.
    /// Loaded from redb on first use, avoids repeated DB lookups.
    session_aliases: std::collections::HashMap<String, String>,
    /// Completed async task results: task_id → (session_key, result_json).
    /// Background task agents write here; main agent checks at turn start.
    pending_task_results: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    /// Sessions in voice mode: auto-TTS reply when user sent voice.
    /// Set when audio attachment detected, cleared by "/text" command.
    voice_mode_sessions: std::collections::HashSet<String>,
    /// Background exec pool — runs long commands without blocking the agent loop.
    exec_pool: Arc<ExecPool>,
}

impl AgentRuntime {
    pub fn new(
        #[allow(clippy::too_many_arguments)] handle: Arc<AgentHandle>,
        config: Arc<RuntimeConfig>,
        providers: Arc<ProviderRegistry>,
        fallback_models: Vec<String>,
        skills: Arc<SkillRegistry>,
        store: Arc<Store>,
        memory: Option<Arc<Mutex<MemoryStore>>>,
        agents: Option<Arc<AgentRegistry>>,
        event_bus: Option<broadcast::Sender<AgentEvent>>,
        spawner: Option<Arc<crate::agent::AgentSpawner>>,
        plugins: Option<Arc<PluginRegistry>>,
        mcp: Option<Arc<crate::mcp::McpRegistry>>,
        notification_tx: Option<tokio::sync::broadcast::Sender<crate::channel::OutboundMessage>>,
    ) -> Self {
        // Populate auth.order so FailoverManager uses the configured profile
        // priority per provider (AGENTS.md §12).
        let auth_order = config
            .model
            .auth
            .as_ref()
            .and_then(|a| a.order.clone())
            .unwrap_or_default();
        let failover = FailoverManager::new(
            auth_order,
            std::collections::HashMap::new(),
            fallback_models,
        );
        let session_aliases = store.db.load_all_aliases().unwrap_or_default();
        let btw_manager = super::btw::BtwManager::new(Some(Arc::clone(&store.db)));
        let live_status = Arc::clone(&handle.live_status);
        // Get max_concurrent from config, default to 4
        let max_concurrent = config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.max_concurrent)
            .unwrap_or(4);
        let exec_pool = ExecPool::new(max_concurrent);
        let rt = Self {
            handle,
            config,
            providers,
            failover,
            skills,
            store,
            memory,
            agents,
            event_bus,
            spawner,
            plugins,
            mcp,
            live_status,
            browser: Arc::new(tokio::sync::Mutex::new(None)),
            sessions: std::collections::HashMap::new(),
            compaction_state: std::collections::HashMap::new(),
            pending_files: std::collections::HashMap::new(),
            runtime_max_file_size: None,
            runtime_max_text_chars: None,
            started_at: std::time::Instant::now(),
            workspace_cache: None,
            btw_manager,
            pending_task_results: Arc::new(std::sync::Mutex::new(Vec::new())),
            voice_mode_sessions: std::collections::HashSet::new(),
            notification_tx,
            opencode_client: Arc::new(tokio::sync::OnceCell::new()),
            claudecode_client: Arc::new(tokio::sync::OnceCell::new()),
            session_aliases,
            exec_pool,
        };

        // Spawn a background task that periodically checks for idle browser
        // sessions and drops them to release Chrome memory.  Runs every 60s.
        {
            let browser_handle = Arc::clone(&rt.browser);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let mut guard = browser_handle.lock().await;
                    if let Some(ref session) = *guard {
                        if session.is_idle_expired() {
                            tracing::info!("browser idle reaper: closing Chrome to free memory");
                            *guard = None;
                        }
                    }
                }
            });
        }

        rt
    }

    // -----------------------------------------------------------------------
    // OpenCode ACP integration
    // -----------------------------------------------------------------------

    /// Get or create the OpenCode ACP client.
    /// Uses the agent's workspace directory as cwd for proper file
    /// organization.
    async fn get_opencode_client(&self) -> Result<crate::acp::client::AcpClient> {
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
            .initialize("rsclaw", env!("RSCLAW_BUILD_VERSION"))
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
    async fn tool_opencode(&self, ctx: &RunContext, args: Value) -> Result<Value> {
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
                                    format!("🔧 {}", title.as_deref().unwrap_or("tool"))
                                }
                                crate::acp::client::SessionEvent::ToolCallCompleted {
                                    result,
                                    ..
                                } => result
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
                                    .unwrap_or_default(),
                                crate::acp::client::SessionEvent::ToolCallFailed {
                                    error, ..
                                } => {
                                    format!("❌ {}", error)
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
                    tracing::debug!(
                        "tool_opencode: sending completion notification, summary_len={}, files={}",
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
                                tracing::debug!("tool_opencode: notification sent successfully")
                            }
                            Err(e) => {
                                tracing::error!("tool_opencode: failed to send notification: {}", e)
                            }
                        }
                    } else {
                        tracing::warn!("tool_opencode: no notification channel available");
                    }
                }
                Err(e) => {
                    tracing::error!("tool_opencode: send_prompt failed: {}", e);
                    if let Some(ref tx) = notif_tx_bg {
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
    async fn get_claudecode_client(&self) -> Result<crate::acp::client::AcpClient> {
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
            .initialize("rsclaw", env!("RSCLAW_BUILD_VERSION"))
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
    async fn tool_claudecode(&self, ctx: &RunContext, args: Value) -> Result<Value> {
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
                                    format!("🔧 {}", title.as_deref().unwrap_or("tool"))
                                }
                                crate::acp::client::SessionEvent::ToolCallCompleted {
                                    result,
                                    ..
                                } => result
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
                                    .unwrap_or_default(),
                                crate::acp::client::SessionEvent::ToolCallFailed {
                                    error, ..
                                } => {
                                    format!("❌ {}", error)
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

    /// Resolve the current model name from agent config with fallback.
    fn resolve_model_name(&self) -> String {
        self.handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.primary.as_deref())
            .or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.primary.as_deref())
            })
            .unwrap_or("anthropic/claude-sonnet-4-6")
            .to_owned()
    }

    // -----------------------------------------------------------------------
    // Plugin hook dispatch (AGENTS.md §20)
    // -----------------------------------------------------------------------

    /// Fire a lifecycle hook on all plugins that subscribe to it.
    /// Errors from individual plugins are logged and swallowed — hooks must
    /// not interrupt the agent loop.
    async fn fire_hook(&self, hook: &str, params: Value) {
        let Some(ref reg) = self.plugins else { return };
        for plugin in reg.all() {
            if !plugin.manifest.hooks.iter().any(|h| h == hook) {
                continue;
            }
            if let Err(e) = plugin.call(hook, params.clone()).await {
                warn!(plugin = %plugin.manifest.name, hook, "hook error: {e:#}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Side-channel quick query (/btw)
    // -----------------------------------------------------------------------

    /// Handle a /btw side-channel query: lightweight LLM call with conversation
    /// context but NO tools. The result is ephemeral -- it is NOT added to
    /// session history and does not affect the main conversation.
    async fn handle_side_query(&mut self, session_key: &str, question: &str) -> Result<AgentReply> {
        // Read current session history (read-only snapshot for context).
        let history: Vec<Message> = self.sessions.get(session_key).cloned().unwrap_or_default();

        // Take last 10 messages for context, then append the question.
        let mut messages: Vec<Message> = history.into_iter().rev().take(10).rev().collect();
        messages.push(Message {
            role: Role::User,
            content: MessageContent::Text(question.to_owned()),
        });

        let model = self.resolve_model_name();
        let req = LlmRequest {
            model,
            messages,
            tools: vec![], // NO tools -- read-only side query
            system: Some(
                "You are answering a quick side question (/btw). Be concise and direct. \
                 You have no tools available. Answer from the conversation context and \
                 your general knowledge only. Reply in the same language as the user's message."
                    .to_owned(),
            ),
            max_tokens: Some(500),
            temperature: None,
            frequency_penalty: None,
            thinking_budget: None,
        };

        let providers = Arc::clone(&self.providers);
        let mut stream = self.failover.call(req, &providers).await?;
        let mut text_buf = String::new();

        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(d)) => text_buf.push_str(&d),
                Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
                Ok(_) => {}
                Err(e) => {
                    warn!("/btw stream error: {e:#}");
                    break;
                }
            }
        }

        // DO NOT persist to session history -- this is ephemeral.
        Ok(AgentReply {
            text: if text_buf.is_empty() {
                "[/btw] (no response)".to_owned()
            } else {
                format!("[/btw] {}", text_buf)
            },
            is_empty: text_buf.is_empty(),
            tool_calls: None,
            images: vec![],
            files: vec![],
            pending_analysis: None,
            was_preparse: false,
        })
    }

    /// Drive a single conversation turn.
    ///
    /// Takes individual fields (not the full `AgentMessage`) so callers can
    /// extract `reply_tx` separately before dispatching.
    pub async fn run_turn(
        &mut self,
        session_key: &str,
        text: &str,
        channel: &str,
        peer_id: &str,
        extra_tools: Vec<ToolDef>,
        images: Vec<super::registry::ImageAttachment>,
        files: Vec<super::registry::FileAttachment>,
    ) -> Result<AgentReply> {
        // Resolve session key alias: if this key maps to a canonical (migrated)
        // key, use that so all messages stay under one session.
        let session_key = self.resolve_session_key(session_key).to_owned();
        let session_key = session_key.as_str();

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
            // Also clear persisted sessions from redb
            for key in self.store.db.list_sessions().unwrap_or_default() {
                let _ = self.store.db.delete_session(&key);
            }

            // Re-inject summaries so agent retains context.
            for (key, msg) in summary_msgs {
                self.sessions.insert(key, vec![msg]);
            }
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
            .map(crate::i18n::resolve_lang)
            .unwrap_or("en");

        // ---------------------------------------------------------------
        // File action: user replies 1/2/3/4 to pending file prompt.
        //   1. 分析并保存  2. 分析后删除  3. 保存(已完成)  4. 删除
        // ---------------------------------------------------------------
        let pending_response = text.trim();
        if (pending_response == "1" || pending_response == "2" || pending_response == "3")
            && let Some(files) = self.pending_files.remove(session_key)
            && !files.is_empty()
        {
            let workspace = self
                .handle
                .config
                .workspace
                .as_deref()
                .or(self.config.agents.defaults.workspace.as_deref())
                .map(expand_tilde)
                .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));
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
                    let mut binary_kept = Vec::new();
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
                            binary_kept.push(pf.filename.clone());
                        }
                        let _ = std::fs::remove_file(&pf.path);
                    }
                    // Binary-only: direct reply, no LLM.
                    if analysis_text.is_empty() {
                        let msg = binary_kept
                            .iter()
                            .map(|f| format!("- {f} (kept in uploads/)"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        return Ok(AgentReply {
                            text: msg,
                            is_empty: false,
                            tool_calls: None,
                            images: vec![],
                            files: vec![],
                            pending_analysis: None,
                            was_preparse: false,
                        });
                    }
                    // Has extractable text: return "analyzing..." immediately,
                    // attach pending analysis for the per-user worker to process.
                    return Ok(AgentReply {
                        text: crate::i18n::t("analyzing", i18n_lang),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: Some(crate::agent::PendingAnalysis {
                            text: analysis_text,
                            session_key: session_key.to_owned(),
                            channel: channel.to_owned(),
                            peer_id: peer_id.to_owned(),
                        }),
                        was_preparse: false,
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
                            crate::i18n::t("no_extractable_deleted", i18n_lang)
                        } else {
                            format!(
                                "{}\n{}",
                                binary_deleted
                                    .iter()
                                    .map(|f| format!("- {f}"))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                crate::i18n::t("no_extractable_deleted", i18n_lang)
                            )
                        };
                        return Ok(AgentReply {
                            text: msg,
                            is_empty: false,
                            tool_calls: None,
                            images: vec![],
                            files: vec![],
                            pending_analysis: None,
                            was_preparse: false,
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
                        text: crate::i18n::t("analyzing", i18n_lang),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: Some(crate::agent::PendingAnalysis {
                            text: analysis_text,
                            session_key: session_key.to_owned(),
                            channel: channel.to_owned(),
                            peer_id: peer_id.to_owned(),
                        }),
                        was_preparse: false,
                    });
                }
                _ => {
                    // 直接删除
                    for pf in &files {
                        let _ = std::fs::remove_file(&pf.path);
                        let _ = std::fs::remove_file(uploads.join(&pf.filename));
                    }
                    return Ok(AgentReply {
                        text: crate::i18n::t("files_deleted", i18n_lang),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                        was_preparse: false,
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
        let preparse = crate::agent::preparse::PreParseEngine::load_with_safety(safety_on);

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
            crate::agent::preparse::PreParseResult::PassThrough => {
                // Normal LLM flow continues below
            }
            crate::agent::preparse::PreParseResult::DirectResponse(response)
                if cmd_permitted(text) =>
            {
                // Handle special directives
                let reply_text = match response.as_str() {
                    "__HELP__" => {
                        let lang = self.config.raw.gateway.as_ref()
                            .and_then(|g| g.language.as_deref())
                            .map(crate::i18n::resolve_lang)
                            .unwrap_or("en");
                        build_help_text_filtered(allowed, lang)
                    }
                    "__VERSION__" => format!("rsclaw {}", env!("RSCLAW_BUILD_VERSION")),
                    "__STATUS__" => {
                        let model = self.resolve_model_name();
                        let sessions = self.sessions.len();
                        let uptime = format_duration(self.started_at.elapsed());
                        let os = if cfg!(target_os = "macos") { "macOS" }
                            else if cfg!(target_os = "linux") {
                                if std::env::var("ANDROID_ROOT").is_ok() { "Android" } else { "Linux" }
                            }
                            else if cfg!(target_os = "windows") { "Windows" }
                            else if cfg!(target_os = "ios") { "iOS" }
                            else { "Unknown" };
                        let ctx_tokens: usize = self.sessions.get(session_key)
                            .map(|msgs| msgs.iter().map(msg_tokens).sum())
                            .unwrap_or(0);
                        self.handle.last_ctx_tokens.store(ctx_tokens, std::sync::atomic::Ordering::Relaxed);
                        let ctx_limit = self.handle.config.model.as_ref()
                            .and_then(|m| m.context_tokens)
                            .or(self.config.agents.defaults.model.as_ref()
                                .and_then(|m| m.context_tokens))
                            .unwrap_or(64000) as usize;
                        format!(
                            "Gateway: running\nOS: {os}\nModel: {model}\nSessions: {sessions}\nContext: ~{:.1}k/{:.0}k tokens\nUptime: {uptime}\nVersion: rsclaw {}",
                            ctx_tokens as f64 / 1000.0,
                            ctx_limit as f64 / 1000.0,
                            env!("RSCLAW_BUILD_VERSION")
                        )
                    }
                    "__HEALTH__" => {
                        let model = self.resolve_model_name();
                        let (prov_name, _) =
                            crate::provider::registry::ProviderRegistry::parse_model(&model);
                        let provider_ok = self.providers.get(prov_name).is_ok();
                        format!(
                            "Health check:\n  Provider ({}): {}\n  Store: ok\n  Agent: {}\n  Version: rsclaw {}",
                            model,
                            if provider_ok { "ok" } else { "unavailable" },
                            self.handle.id,
                            env!("RSCLAW_BUILD_VERSION"),
                        )
                    }
                    "__UPTIME__" => format_duration(self.started_at.elapsed()),
                    "__MODELS__" => {
                        let current = self.resolve_model_name();
                        let mut lines = vec![format!("Current model: {current}")];
                        lines.push(String::new());
                        lines.push("Registered providers:".to_owned());
                        for name in self.providers.names() {
                            lines.push(format!("  {name}"));
                        }
                        lines.join("\n")
                    }
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
                                let context_tokens = self.config.agents.defaults.context_tokens.unwrap_or(64_000) as usize;
                                let cfg = self.config.agents.defaults.compaction.clone().unwrap_or_default();
                                let default_transcript = (context_tokens * 7 / 10).max(16_000);
                                let max_transcript = cfg.max_transcript_tokens.map(|t| t as usize).unwrap_or(default_transcript);
                                // Render transcript (reuse the same logic as compaction).
                                let transcript = Self::msgs_to_text_static(msgs, max_transcript);
                                let compaction_model = cfg.model.as_deref().unwrap_or(&model);
                                self.compact_single(compaction_model, &transcript).await
                            }
                        } else {
                            None
                        };

                        self.sessions.remove(session_key);
                        if let Err(e) = self.store.db.delete_session(session_key) {
                            warn!("failed to clear persisted session: {e:#}");
                        }
                        if let Some(summary) = summary_text {
                            let msg = Message {
                                role: crate::provider::Role::User,
                                content: crate::provider::MessageContent::Text(
                                    format!("[Session summary before /clear]\n{summary}")
                                ),
                            };
                            self.sessions.insert(session_key.to_owned(), vec![msg]);
                        }
                        "Session cleared.".to_owned()
                    }
                    "__COMPACT__" => {
                        // Manual compaction: force compress + save summary to memory.
                        let model = self.resolve_model_name();
                        self.compact_force(session_key, &model).await;
                        // Extract summary from the compacted session for memory storage.
                        // Look for the compaction-tagged System message specifically.
                        const COMPACTION_TAG: &str = "[Conversation history compacted";
                        if let Some(msgs) = self.sessions.get(session_key) {
                            let summary_text = msgs.iter().find_map(|m| {
                                if m.role == crate::provider::Role::System {
                                    let text = match &m.content {
                                        crate::provider::MessageContent::Text(s) => s.clone(),
                                        crate::provider::MessageContent::Parts(parts) => parts.iter().filter_map(|p| {
                                            if let crate::provider::ContentPart::Text { text } = p { Some(text.as_str()) } else { None }
                                        }).collect::<Vec<_>>().join(" "),
                                    };
                                    if text.starts_with(COMPACTION_TAG) { Some(text) } else { None }
                                } else { None }
                            });
                            if let Some(summary) = summary_text {
                                if let Some(ref mem) = self.memory {
                                    // UTF-8 safe truncation.
                                    let truncated: String = summary.chars().take(2000).collect();
                                    let mem_text = format!("Session compaction summary:\n{truncated}");
                                    let now = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs() as i64)
                                        .unwrap_or(0);
                                    let doc = crate::agent::memory::MemoryDoc {
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
                                    };
                                    match mem.lock().await.add(doc).await {
                                        Ok(_) => info!("compact: summary saved to memory ({} chars)", mem_text.len()),
                                        Err(e) => warn!("compact: failed to save to memory: {e}"),
                                    }
                                }
                                "✓ Session compacted and saved to memory.".to_owned()
                            } else {
                                "✓ Session compacted (no summary to save).".to_owned()
                            }
                        } else {
                            "Nothing to compact.".to_owned()
                        }
                    }
                    "__ABORT__" => {
                        // Set abort flag for this session to interrupt running turn
                        let resolved_key = self.resolve_session_key(session_key);
                        let flags = self.handle.abort_flags.write().unwrap();
                        if let Some(flag) = flags.get(resolved_key) {
                            flag.store(true, std::sync::atomic::Ordering::SeqCst);
                            "Abort signal sent. The running task will stop shortly.".to_owned()
                        } else {
                            "No active task found for this session.".to_owned()
                        }
                    }
                    "__RESET__" => {
                        // Clear in-memory cache AND redb session data.
                        let key = self.resolve_session_key(session_key).to_owned();
                        self.sessions.remove(&key);
                        let _ = self.store.db.delete_session(&key);
                        self.voice_mode_sessions.remove(&key);
                        "Session reset.".to_owned()
                    }
                    "__TEXT_MODE__" => {
                        self.voice_mode_sessions.remove(session_key);
                        let zh = crate::i18n::default_lang() == "zh";
                        if zh { "已切换到文字回复模式。".to_owned() }
                        else { "Switched to text reply mode.".to_owned() }
                    }
                    "__VOICE_MODE__" => {
                        self.voice_mode_sessions.insert(session_key.to_owned());
                        let zh = crate::i18n::default_lang() == "zh";
                        if zh { "已切换到语音回复模式。".to_owned() }
                        else { "Switched to voice reply mode.".to_owned() }
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
                            let mut lines = vec![
                                format!("📊 Context: {} messages, ~{} tokens", msgs.len(), total_tokens),
                            ];
                            for (i, msg) in msgs[start..].iter().enumerate() {
                                let role = match msg.role {
                                    crate::provider::Role::User => "You",
                                    crate::provider::Role::Assistant => "AI",
                                    crate::provider::Role::System => "Sys",
                                    crate::provider::Role::Tool => "Tool",
                                };
                                let text = match &msg.content {
                                    crate::provider::MessageContent::Text(s) => s.clone(),
                                    crate::provider::MessageContent::Parts(parts) => parts
                                        .iter()
                                        .filter_map(|p| {
                                            if let crate::provider::ContentPart::Text { text } = p {
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
                                    let end = key.char_indices().nth(30).map(|(i, _)| i).unwrap_or(key.len());
                                    &key[..end]
                                } else { key };
                                lines.push(format!("  {} ({} messages)", short_key, msgs.len()));
                            }
                            lines.join("\n")
                        }
                    }
                    "__CRON_LIST__" => {
                        if let Some(ref cron_cfg) = self.config.ops.cron {
                            if let Some(ref jobs) = cron_cfg.jobs {
                                if jobs.is_empty() {
                                    "No cron jobs configured.".to_owned()
                                } else {
                                    let mut lines = vec!["Cron jobs:".to_owned()];
                                    for job in jobs {
                                        let enabled = job.enabled.unwrap_or(true);
                                        let status = if enabled { "" } else { " (disabled)" };
                                        let agent = job.agent_id.as_deref().unwrap_or("main");
                                        let msg_preview = if job.message.len() > 50 {
                                            let end = job.message.char_indices().nth(47).map(|(i, _)| i).unwrap_or(job.message.len());
                                            format!("{}...", &job.message[..end])
                                        } else {
                                            job.message.clone()
                                        };
                                        lines.push(format!(
                                            "  [{}] {} -> {} \"{}\"{}",
                                            job.id, job.schedule, agent, msg_preview, status
                                        ));
                                    }
                                    lines.join("\n")
                                }
                            } else {
                                "No cron jobs configured.".to_owned()
                            }
                        } else {
                            "No cron jobs configured.".to_owned()
                        }
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
                    // --- Background context (/ctx, formerly /btw) ---
                    s if s.starts_with("__CTX_ADD__:") => {
                        let content = s.strip_prefix("__CTX_ADD__:").unwrap_or("");
                        let id = self
                            .btw_manager
                            .add(
                                content,
                                super::btw::BtwScope::Session(session_key.to_owned()),
                                None,
                            )
                            .await;
                        let lang = crate::i18n::default_lang();
                        crate::i18n::t_fmt("btw_added", lang, &[("id", &id.to_string())])
                    }
                    s if s.starts_with("__CTX_TTL__:") => {
                        let rest = s.strip_prefix("__CTX_TTL__:").unwrap_or("");
                        let (turns_str, content) = rest.split_once(':').unwrap_or(("0", rest));
                        let turns: u32 = turns_str.parse().unwrap_or(0);
                        let id = self
                            .btw_manager
                            .add(
                                content,
                                super::btw::BtwScope::Session(session_key.to_owned()),
                                Some(turns),
                            )
                            .await;
                        let lang = crate::i18n::default_lang();
                        crate::i18n::t_fmt(
                            "btw_added_ttl",
                            lang,
                            &[("id", &id.to_string()), ("turns", &turns.to_string())],
                        )
                    }
                    s if s.starts_with("__CTX_GLOBAL__:") => {
                        if !is_default {
                            format!("Command not available on agent `{}`.", self.handle.id)
                        } else {
                            let content = s.strip_prefix("__CTX_GLOBAL__:").unwrap_or("");
                            let id = self.btw_manager.add(
                                content,
                                super::btw::BtwScope::Global,
                                None,
                            ).await;
                            let lang = crate::i18n::default_lang();
                            crate::i18n::t_fmt("btw_added_global", lang, &[("id", &id.to_string())])
                        }
                    }
                    "__CTX_LIST__" => {
                        let entries = self.btw_manager.list(session_key, channel).await;
                        if entries.is_empty() {
                            let lang = crate::i18n::default_lang();
                            crate::i18n::t("btw_list_empty", lang)
                        } else {
                            let mut lines = Vec::new();
                            for e in &entries {
                                let ttl_info = if let Some(remaining) = e.remaining_turns {
                                    format!("{remaining} turns left")
                                } else {
                                    "permanent".to_owned()
                                };
                                let scope_info = match &e.scope {
                                    super::btw::BtwScope::Session(_) => "",
                                    super::btw::BtwScope::Channel(_) => " [channel]",
                                    super::btw::BtwScope::Global => " [global]",
                                };
                                lines.push(format!(
                                    "[{}] ({}{}) {}",
                                    e.id, ttl_info, scope_info, e.content
                                ));
                            }
                            lines.join("\n")
                        }
                    }
                    "__CTX_CLEAR__" => {
                        self.btw_manager.clear(Some(session_key)).await;
                        let lang = crate::i18n::default_lang();
                        crate::i18n::t("btw_cleared", lang)
                    }
                    s if s.starts_with("__CTX_REMOVE__:") => {
                        let id_str = s.strip_prefix("__CTX_REMOVE__:").unwrap_or("0");
                        let id: u32 = id_str.parse().unwrap_or(0);
                        let lang = crate::i18n::default_lang();
                        if self.btw_manager.remove(id).await {
                            crate::i18n::t_fmt("btw_removed", lang, &[("id", &id.to_string())])
                        } else {
                            crate::i18n::t_fmt("btw_not_found", lang, &[("id", &id.to_string())])
                        }
                    }
                    "__CTX_USAGE__" => {
                        "Usage:\n  /ctx <text>           Add context (session)\n  /ctx --ttl <N> <text> Add context (expires in N turns)\n  /ctx --global <text>  Add global context\n  /ctx --list           List entries\n  /ctx --remove <id>    Remove entry\n  /ctx --clear          Clear all".to_owned()
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
                            was_preparse: true,
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
                        was_preparse: true,
                    });
                }
                // Fall through to LLM for unhandled directives
            }
            crate::agent::preparse::PreParseResult::ToolCall { tool, args }
                if cmd_permitted(text) =>
            {
                // Group chat safety: block dangerous preparse commands (/run, /ls, /cat, etc.)
                let is_group = session_key.contains(":group:");
                if is_group && matches!(tool.as_str(), "execute_command" | "exec" | "read_file" | "read" | "write_file" | "write") {
                    return Ok(AgentReply {
                        text: "[Blocked] Shell/file commands are not allowed in group chats for security.".to_owned(),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                        was_preparse: true,
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
                            exec_pool: Arc::clone(&self.exec_pool),
                            loop_detector: crate::agent::loop_detection::LoopDetector::default(),
                            has_images: false,
                            user_msg_with_images: None,
                            parse_error_count: 0,
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
                            was_preparse: true,
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
                            was_preparse: true,
                        });
                    }
                }
            }
            crate::agent::preparse::PreParseResult::Blocked(reason) => {
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
                        was_preparse: true,
                    });
                }
                // Safety off: fall through to execute anyway
            }
            crate::agent::preparse::PreParseResult::NeedsConfirm { command, reason } => {
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
                        was_preparse: true,
                    });
                }
                // Safety off: fall through to execute anyway
            }
            // Preparse matched a command but cmd_permitted denied it: block instead of falling
            // through to LLM
            crate::agent::preparse::PreParseResult::DirectResponse(_)
            | crate::agent::preparse::PreParseResult::ToolCall { .. } => {
                return Ok(AgentReply {
                    text: format!("Command not available on agent `{}`.", self.handle.id),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    was_preparse: true,
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
                was_preparse: false,
            });
        }

        // ---------------------------------------------------------------
        // File attachment: auto-detect video/audio for direct transcription.
        // For doubao (Responses API): convert video FileAttachments to
        // ImageAttachments so they go through Files API → input_video.
        // This is unified here so all channels benefit without changes.
        // ---------------------------------------------------------------
        let cur_model = self.resolve_model_name();
        let is_doubao = cur_model.to_lowercase().contains("doubao")
            || cur_model.to_lowercase().contains("seed");
        let mut files = files;
        let mut images = images;
        let mut text_override: Option<String> = None;
        if is_doubao {
            let mut remaining = Vec::new();
            for f in files {
                if crate::channel::is_video_attachment(&f.mime_type, &f.filename) {
                    // NOTE: During base64 encoding, both f.data and the b64 string
                    // (~133% of original) coexist in memory. The 100 MB upload limit
                    // (MAX_UPLOAD_SIZE) bounds the worst case to ~233 MB peak.
                    use base64::Engine;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&f.data);
                    let mime = if f.mime_type.is_empty() {
                        "video/mp4"
                    } else {
                        &f.mime_type
                    };
                    let data_uri = format!("data:{mime};base64,{b64}");
                    images.push(super::registry::ImageAttachment {
                        data: data_uri,
                        mime_type: mime.to_owned(),
                    });
                    if text.is_empty() && text_override.is_none() {
                        text_override = Some(crate::i18n::t(
                            "describe_video",
                            crate::i18n::default_lang(),
                        ));
                    }
                    info!(
                        size = f.data.len(),
                        "video FileAttachment → ImageAttachment for vision"
                    );
                } else {
                    remaining.push(f);
                }
            }
            files = remaining;
        }
        let text = text_override.as_deref().unwrap_or(text);
        let (media_files, regular_files): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| {
            crate::channel::is_video_attachment(&f.mime_type, &f.filename)
                || crate::channel::is_audio_attachment(&f.mime_type, &f.filename)
        });
        let files = regular_files;

        if !media_files.is_empty() {
            // Auto-enable voice mode when user sends audio (not video).
            let has_audio = media_files.iter().any(|f|
                crate::channel::is_audio_attachment(&f.mime_type, &f.filename)
                && !crate::channel::is_video_attachment(&f.mime_type, &f.filename)
            );
            if has_audio {
                self.voice_mode_sessions.insert(session_key.to_owned());
                debug!(session = session_key, "voice mode enabled (audio attachment detected)");
            }
            let mut transcriptions = Vec::new();
            for mf in &media_files {
                if let Some(t) = extract_audio_text(&mf.data, &mf.filename.to_lowercase()).await {
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
                    extra_tools,
                    images,
                    vec![],
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
                    extra_tools,
                    images,
                    files,
                ))
                .await;
            }
        }

        // ---------------------------------------------------------------
        // File attachment: auto-save + show 3-option menu
        // ---------------------------------------------------------------
        if !files.is_empty() {
            let ws = agent_cfg
                .workspace
                .as_deref()
                .or(self.config.agents.defaults.workspace.as_deref())
                .map(expand_tilde)
                .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));
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
                    crate::i18n::t_fmt("file_size_exceeded", i18n_lang, &[("limit", &limit_str)]);
                let adjust = crate::i18n::t("file_size_adjust", i18n_lang);
                return Ok(AgentReply {
                    text: format!("{msg}\n{}\n\n{adjust}", rejected.join("\n")),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    was_preparse: false,
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
                    text: crate::i18n::t_fmt(
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
                    was_preparse: false,
                });
            }

            let mut file_info = Vec::new();
            for file in files {
                let dest = uploads.join(&file.filename);
                let size = file.data.len();
                let _ = std::fs::write(&dest, &file.data);

                let extracted = extract_file_text(&file.filename, &file.data).await;
                let has_text = extracted.is_some();
                let est_tokens = extracted.as_ref().map(|t| estimate_tokens(t)).unwrap_or(0);

                file_info.push((file.filename.clone(), size, has_text, est_tokens));

                // Store pending for later analysis
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
                self.pending_files
                    .entry(session_key.to_owned())
                    .or_default()
                    .push(PendingFile {
                        filename: file.filename,
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
                        crate::i18n::t_fmt(
                            "file_analyzable",
                            i18n_lang,
                            &[("tokens", &tokens.to_string())],
                        )
                    } else {
                        crate::i18n::t("file_binary", i18n_lang)
                    };
                    format!("- {name} ({size_str}, {analysis})")
                })
                .collect::<Vec<_>>()
                .join("\n");

            let saved_msg = crate::i18n::t_fmt(
                "file_saved",
                i18n_lang,
                &[("count", &file_info.len().to_string())],
            );
            let any_analyzable = file_info.iter().any(|(_, _, has_text, _)| *has_text);
            let menu_msg = if any_analyzable {
                crate::i18n::t("file_menu", i18n_lang)
            } else {
                // Binary only -- simplified menu.
                "1. Keep\n2. Delete".to_owned()
            };
            let reply = format!("{saved_msg}\n{file_list}\n\n{menu_msg}");
            return Ok(AgentReply {
                text: reply,
                is_empty: false,
                tool_calls: None,
                images: vec![],
                files: vec![],
                pending_analysis: None,
                was_preparse: false,
            });
        }

        // (Old two-layer image/text gate removed -- files handled above)

        // Workspace path — expand leading `~/` so dynamically spawned agents work.
        let workspace = agent_cfg
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

        // Load workspace context (cached -- only re-reads files whose mtime changed).
        let ws_ctx = {
            let cache = self
                .workspace_cache
                .get_or_insert_with(|| crate::agent::workspace::WorkspaceCache::new(&workspace));
            cache.load(
                SessionType::Normal,
                true,
                DEFAULT_MAX_CHARS_PER_FILE,
                DEFAULT_TOTAL_MAX_CHARS,
            )
        };

        // Build system prompt.
        let mut system_prompt = build_system_prompt(&ws_ctx, &self.skills, &self.config.raw);

        // DEBUG: dump full system prompt to file for inspection
        if std::env::var("RSCLAW_DUMP_PROMPT").is_ok() {
            let dump_path = crate::config::loader::base_dir().join("debug_system_prompt.txt");
            let _ = std::fs::write(&dump_path, &system_prompt);
            tracing::info!(path = %dump_path.display(), len = system_prompt.len(), "dumped system prompt");
        }

        // Auto-Recall (AGENTS.md §31): hybrid vector+BM25 recall before prompt.
        if let Some(ref mem) = self.memory
            && !text.trim().is_empty()
        {
            let scope = format!("agent:{}", self.handle.id);
            let mem_cfg = &self.config.raw.memory;
            let recall_top_k = mem_cfg.as_ref().and_then(|m| m.recall_top_k).unwrap_or(10);
            let recall_final_k = mem_cfg.as_ref().and_then(|m| m.recall_final_k).unwrap_or(5);
            // 1. Vector recall.
            let vec_hits = {
                let mut guard = mem.lock().await;
                guard
                    .search(text, Some(&scope), recall_top_k)
                    .await
                    .unwrap_or_default()
            };
            // 2. BM25 recall (tantivy).
            let bm25_hits = self
                .store
                .search
                .search(text, Some(&scope), recall_top_k)
                .unwrap_or_default();
            // 3. Fuse with Reciprocal Rank Fusion (k=60), return top final_k.
            let results = rrf_fuse(vec_hits, bm25_hits, recall_final_k);
            if !results.is_empty() {
                let now_ts = chrono::Utc::now().timestamp();
                let mem_block = format!(
                    "<relevant-memories>\n{}\n</relevant-memories>",
                    results
                        .iter()
                        .map(|d| {
                            let age = memory_age_label(now_ts, d.created_at);
                            format!("- [{}] {} ({})", d.kind, d.display_text(), age)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                if system_prompt.is_empty() {
                    system_prompt = mem_block;
                } else {
                    system_prompt = format!("{system_prompt}\n\n{mem_block}");
                }
            }
        }

        // Background context injection (/ctx).
        let btw_block = self
            .btw_manager
            .to_prompt_block_relevant(session_key, channel, text)
            .await;
        if !btw_block.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&btw_block);
        }

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
        let model = agent_cfg
            .model
            .as_ref()
            .and_then(|m| m.primary.as_deref())
            .or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.primary.as_deref())
            })
            .unwrap_or("anthropic/claude-sonnet-4-6")
            .to_owned();

        // Channel-aware formatting hints for IM channels (feishu, dingtalk, etc.)
        if channel == "feishu" || channel == "dingtalk" || channel == "wecom" {
            system_prompt.push_str(concat!(
                "\n\n[Output format rules for IM chat]\n",
                "- Never use Markdown headings (#, ##, ###).\n",
                "- Use **bold text** or 【section title】 for sections.\n",
                "- Use 1. or - for lists.\n",
                "- Use > for important quotes.\n",
                "- Do NOT use Markdown tables (|---|). Use \"label: value\" format instead.\n",
                "\n[Data integrity rules - CRITICAL]\n",
                "- NEVER truncate or shorten ANY text, strings, numbers, or identifiers.\n",
                "- Copy ALL values EXACTLY: UUIDs, IDs, IP addresses, paths, URLs, code, data.\n",
                "- UUIDs: 36 chars (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx).\n",
                "- IP addresses: complete (127.0.0.1 not 7.0.0.1).\n",
                "- If you see truncated data in context, report it as incomplete.\n",
                "\n[Tool usage rules]\n",
                "- For ANY question about real-time data (prices, weather, news, dates, events), you MUST call web_search first. NEVER answer from memory.\n",
                "- For shell commands, file operations, use exec/read/write tools.\n",
            ));
        }

        // Build tool list from skills and registered agents (local + remote).
        // Tool selection: toolsEnabled -> toolset level -> tools whitelist
        let model_cfg = self.handle.config.model.as_ref().or(self
            .config
            .agents
            .defaults
            .model
            .as_ref());
        let tools_enabled = model_cfg.and_then(|m| m.tools_enabled).unwrap_or(true);

        let tools = if !tools_enabled {
            vec![]
        } else {
            // Build full tool list first
            let mut all = build_tool_list(
                &self.skills,
                self.agents.as_deref(),
                &self.handle.id,
                &self.config.agents.external,
            );
            all.extend(extra_tools.iter().cloned());
            if let Some(ref mcp) = self.mcp {
                all.extend(mcp.all_tool_defs().await);
            }

            // Apply toolset level + custom tools list
            // Default agent uses "full", others use "standard"
            let is_default = self.handle.config.default.unwrap_or(false);
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

            // Group chat safety: strip dangerous tools to prevent exec via LLM
            let is_group = session_key.contains(":group:");
            if is_group {
                const GROUP_BLOCKED_TOOLS: &[&str] = &["execute_command", "exec", "read_file", "read", "write_file", "write", "computer_use"];
                all.retain(|t| !GROUP_BLOCKED_TOOLS.contains(&t.name.as_str()));
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

            all
        };

        // Check vision support before loading session (avoids borrow conflict).
        let vision = model_supports_vision(&model, &self.config);

        // Load or initialise session history.
        let session_messages = self.load_session(session_key);

        // Append user message to session.
        let images = if vision {
            images
        } else {
            if !images.is_empty() {
                info!(
                    "model {model} does not support vision, stripping {} image(s)",
                    images.len()
                );
            }
            vec![]
        };
        // Compress images before sending to LLM to save tokens.
        // Skip compression for video attachments — they go to Files API directly.
        let compressed_images: Vec<_> = images
            .iter()
            .filter_map(|img| {
                if img.mime_type.starts_with("video/") {
                    // Pass video through without compression
                    return Some(img.clone());
                }
                compress_image_for_llm(&img.data).map(|data| super::registry::ImageAttachment {
                    data,
                    mime_type: "image/jpeg".to_owned(),
                })
            })
            .collect();
        let content = if compressed_images.is_empty() && images.is_empty() {
            MessageContent::Text(text.to_owned())
        } else {
            let imgs = if compressed_images.is_empty() {
                &images
            } else {
                &compressed_images
            };
            let mut parts = vec![ContentPart::Text {
                text: text.to_owned(),
            }];
            for img in imgs {
                parts.push(ContentPart::Image {
                    url: img.data.clone(),
                });
            }
            MessageContent::Parts(parts)
        };
        let user_msg = Message {
            role: Role::User,
            content,
        };
        // Store stripped version in session (no image base64 to avoid bloating).
        // The full user_msg with images is only used for the current LLM call.
        let persist_msg = if images.is_empty() {
            user_msg.clone()
        } else {
            Message {
                role: Role::User,
                content: MessageContent::Text(format!("{text} [image attached]")),
            }
        };
        session_messages.push(persist_msg.clone());
        let _ = self.store.db.append_message(
            session_key,
            &serde_json::to_value(&persist_msg).unwrap_or_default(),
        );

        // Timeout wrapper.
        let timeout_secs = self
            .config
            .agents
            .defaults
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS as u32) as u64;

        // Get or create abort flag for this session.
        let abort_flag: Arc<AtomicBool> = {
            let mut flags = self.handle.abort_flags.write()
                .expect("abort_flags lock poisoned");
            Arc::clone(flags.entry(session_key.to_string()).or_insert_with(|| {
                Arc::new(AtomicBool::new(false))
            }))
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
                was_preparse: false,
            });
        }

        let mut ctx = RunContext {
            agent_id: self.handle.id.clone(),
            session_key: session_key.to_owned(),
            channel: channel.to_owned(),
            peer_id: peer_id.to_owned(),
            chat_id: String::new(),
            exec_pool: Arc::clone(&self.exec_pool),
            loop_detector: {
                let ld_cfg = self
                    .config
                    .ext
                    .tools
                    .as_ref()
                    .and_then(|t| t.loop_detection.as_ref());
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
            has_images: !images.is_empty(),
            user_msg_with_images: if !images.is_empty() {
                Some(user_msg.clone())
            } else {
                None
            },
            parse_error_count: 0,
        };

        // Check for pending exec results from background tasks started in previous turns.
        let pending_results = self.exec_pool.collect_pending_for_session(session_key).await;
        if !pending_results.is_empty() {
            tracing::info!(
                session_key = %session_key,
                count = pending_results.len(),
                task_ids = ?pending_results.iter().map(|r| &r.task_id).collect::<Vec<_>>(),
                "exec_pool: collected pending results for session"
            );

            // Collect all tool_use_ids currently in session history
            let session_tool_ids: std::collections::HashSet<String> = {
                let sess = self.sessions.get(session_key);
                if let Some(sess) = sess {
                    sess.iter()
                        .filter_map(|m| {
                            if m.role == Role::Assistant {
                                match &m.content {
                                    MessageContent::Parts(parts) => {
                                        Some(parts.iter().filter_map(|p| {
                                            if let ContentPart::ToolUse { id, .. } = p {
                                                Some(id.clone())
                                            } else {
                                                None
                                            }
                                        }).collect::<Vec<_>>())
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        })
                        .flatten()
                        .collect()
                } else {
                    std::collections::HashSet::new()
                }
            };
            tracing::debug!(
                session_key = %session_key,
                tool_ids = ?session_tool_ids,
                "exec_pool: existing ToolUse IDs in session history"
            );

            // Find existing "running" ToolResults to replace with final results
            // (exec tool returns {status: "running"} immediately, then injects final result later)
            let running_tool_result_ids: std::collections::HashSet<String> = {
                let sess = self.sessions.get(session_key);
                if let Some(sess) = sess {
                    sess.iter()
                        .filter_map(|m| {
                            if m.role == Role::Tool {
                                match &m.content {
                                    MessageContent::Parts(parts) => {
                                        for p in parts {
                                            if let ContentPart::ToolResult { tool_use_id, content, .. } = p {
                                                // Check if this is a "running" status result
                                                if content.contains("\"status\": \"running\"") {
                                                    tracing::debug!(
                                                        tool_use_id = %tool_use_id,
                                                        content_preview = %content.chars().take(100).collect::<String>(),
                                                        "exec_pool: found running status ToolResult to replace"
                                                    );
                                                    return Some(tool_use_id.clone());
                                                }
                                            }
                                        }
                                        None
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    std::collections::HashSet::new()
                }
            };
            tracing::info!(
                session_key = %session_key,
                running_ids = ?running_tool_result_ids,
                "exec_pool: running status ToolResult IDs to potentially replace"
            );

            if let Some(sess) = self.sessions.get_mut(session_key) {
                // Remove existing "running" ToolResults for the same tool_call_ids
                let ids_to_replace: std::collections::HashSet<String> = pending_results
                    .iter()
                    .map(|r| r.tool_call_id.clone())
                    .filter(|id| running_tool_result_ids.contains(id))
                    .collect();

                if !ids_to_replace.is_empty() {
                    tracing::info!(
                        session_key = %session_key,
                        ids = ?ids_to_replace,
                        "removing running status ToolResults before injecting final results"
                    );
                    // Retain messages that are NOT ToolResult with running status for these ids
                    sess.retain(|m| {
                        if m.role == Role::Tool {
                            match &m.content {
                                MessageContent::Parts(parts) => {
                                    for p in parts {
                                        if let ContentPart::ToolResult { tool_use_id, content, .. } = p {
                                            if ids_to_replace.contains(tool_use_id) && content.contains("\"status\": \"running\"") {
                                                return false; // remove this message
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        true // keep all other messages
                    });
                }

                for result in pending_results {
                    let tool_call_id = result.tool_call_id.clone();

                    tracing::info!(
                        task_id = %result.task_id,
                        tool_call_id = %tool_call_id,
                        command = %result.command,
                        exit_code = ?result.exit_code,
                        stdout_len = result.stdout.len(),
                        stderr_len = result.stderr.len(),
                        in_history = session_tool_ids.contains(&tool_call_id),
                        "exec_pool: injecting result into session"
                    );

                    // If ToolUse is not in history, inject a synthetic one first
                    if !session_tool_ids.contains(&tool_call_id) {
                        tracing::warn!(
                            task_id = %result.task_id,
                            tool_call_id = %tool_call_id,
                            command = %result.command,
                            "exec_pool: ToolUse not found in history, injecting synthetic ToolUse"
                        );
                        // Inject synthetic ToolUse (assistant message)
                        sess.push(Message {
                            role: Role::Assistant,
                            content: MessageContent::Parts(vec![
                                ContentPart::ToolUse {
                                    id: tool_call_id.clone(),
                                    name: "exec".to_owned(),
                                    input: serde_json::json!({
                                        "command": result.command,
                                        "_synthetic": true,
                                        "_note": "Background exec from previous session (ToolUse reconstructed for context)"
                                    }),
                                },
                            ]),
                        });
                    }

                    let is_error = result.exit_code.map(|c| c != 0).unwrap_or(true);
                    let content = serde_json::json!({
                        "exit_code": result.exit_code,
                        "stdout": result.stdout,
                        "stderr": result.stderr,
                    }).to_string();
                    tracing::debug!(
                        tool_call_id = %tool_call_id,
                        is_error = is_error,
                        content_len = content.len(),
                        "exec_pool: pushing ToolResult message"
                    );
                    sess.push(Message {
                        role: Role::Tool,
                        content: MessageContent::Parts(vec![
                            crate::provider::ContentPart::ToolResult {
                                tool_use_id: tool_call_id,
                                content,
                                is_error: Some(is_error),
                            },
                        ]),
                    });
                }
            }
        }

        // On-demand skill injection: match user text against skill
        // descriptions and inject full prompt for relevant skills.
        {
            let matched = match_skills(text, &self.skills);
            if !matched.is_empty() {
                let skill_prompts: String = matched
                    .iter()
                    .map(|s| format!(
                        "<active_skill name=\"{}\" version=\"{}\">\n{}\n</active_skill>",
                        s.name,
                        s.version.as_deref().unwrap_or(""),
                        s.prompt.trim(),
                    ))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                system_prompt.push_str(&format!(
                    "\n\n## Active Skills (matched to current request)\n\
                     Follow these skill instructions carefully:\n\n{skill_prompts}"
                ));
                info!(
                    skills = ?matched.iter().map(|s| &s.name).collect::<Vec<_>>(),
                    "skills matched for turn"
                );
            }
        }

        let reply = time::timeout(
            Duration::from_secs(timeout_secs),
            self.agent_loop(&mut ctx, &model, &system_prompt, tools, extra_tools, abort_flag.clone()),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "agent `{}` turn timed out after {timeout_secs}s",
                self.handle.id
            )
        })??;

        // Update live status: turn finished.
        if let Ok(mut status) = self.live_status.try_write() {
            status.state = "idle".to_owned();
            status.current_task.clear();
            status.text_preview.clear();
        }
        self.handle.session_count.store(self.sessions.len(), std::sync::atomic::Ordering::Relaxed);

        // Append to JSONL transcript (AGENTS.md §20 step 11).
        self.append_transcript(session_key, text, &reply.text).await;

        // Auto-Capture (AGENTS.md §31): persist user message as memory note.
        if let Some(ref mem) = self.memory
            && text.len() > 20
            && !reply.text.starts_with(NO_REPLY_TOKEN)
        {
            let doc_id = Uuid::new_v4().to_string();
            let doc_scope = format!("agent:{}", self.handle.id);
            let doc = MemoryDoc {
                id: doc_id.clone(),
                scope: doc_scope.clone(),
                kind: "note".to_owned(),
                text: text.to_owned(),
                created_at: 0, // backfilled in MemoryStore::add()
                accessed_at: 0,
                access_count: 0,
                importance: 0.5,
                vector: vec![],
                tier: Default::default(),
                abstract_text: None,
                overview_text: None,
            };
            let _ = mem.lock().await.add(doc).await;
            // Also index in tantivy BM25 for hybrid search.
            if let Err(e) = self
                .store
                .search
                .index_memory_doc(&doc_id, &doc_scope, "note", text)
            {
                tracing::warn!("BM25 index failed for auto-capture doc: {e:#}");
            }
        }

        // Compaction check (AGENTS.md §15).
        self.compact_if_needed(session_key, &model).await;

        // Tick ctx TTL counters after each turn.
        self.btw_manager.tick_turn(session_key).await;

        // Auto-TTS: if session is in voice mode, generate audio for the reply.
        let mut reply = reply;
        if self.voice_mode_sessions.contains(session_key)
            && !reply.text.is_empty()
            && !reply.is_empty
            && !reply.was_preparse
        {
            match self.generate_tts_audio(&reply.text).await {
                Ok(audio_path) => {
                    let mime = if audio_path.ends_with(".wav") { "audio/wav" }
                        else if audio_path.ends_with(".mp3") { "audio/mpeg" }
                        else { "audio/wav" };
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

    /// Resolve a session key through the alias table.
    /// If the key has an alias, returns the canonical (old) key so all data
    /// stays under one key. Otherwise returns the key unchanged.
    fn resolve_session_key<'a>(&'a self, session_key: &'a str) -> &'a str {
        if let Some(canonical) = self.session_aliases.get(session_key) {
            canonical.as_str()
        } else {
            session_key
        }
    }

    /// Load session history from in-memory cache, falling back to redb.
    /// Session key should already be resolved through `resolve_session_key`
    /// (done in `run_turn`) so aliases are transparent.
    fn load_session(&mut self, session_key: &str) -> &mut Vec<Message> {
        if !self.sessions.contains_key(session_key) {
            let history = self
                .store
                .db
                .load_messages(session_key)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| serde_json::from_value::<Message>(v).ok())
                .collect::<Vec<_>>();
            self.sessions.insert(session_key.to_owned(), history);
        }
        self.sessions.get_mut(session_key).expect("just inserted")
    }

    // -----------------------------------------------------------------------
    // Core agent loop
    // -----------------------------------------------------------------------

    async fn agent_loop(
        &mut self,
        ctx: &mut RunContext,
        model: &str,
        system_prompt: &str,
        tools: Vec<ToolDef>,
        extra_tools: Vec<ToolDef>,
        abort_flag: Arc<AtomicBool>,
    ) -> Result<AgentReply> {
        let pruning_cfg = self.config.agents.defaults.context_pruning.clone();

        // Resolve context budget (tokens) for history trimming.
        // Priority: agent model config > defaults.contextTokens >
        // defaults.model.contextTokens > 128000
        let context_tokens = self
            .handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.context_tokens)
            .or(self.config.agents.defaults.context_tokens)
            .or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.context_tokens)
            })
            .unwrap_or(64_000) as usize;

        let mut tool_images: Vec<String> = Vec::new();
        let mut tool_files: Vec<(String, String, String)> = Vec::new();

        // Inject completed async task results into the session.
        {
            let mut pending = self.pending_task_results.lock().unwrap_or_else(|e| e.into_inner());
            let completed: Vec<(String, String, String)> = pending
                .drain(..)
                .filter(|(_, sk, _)| sk == &ctx.session_key)
                .collect();
            drop(pending);
            if !completed.is_empty() {
                if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                    for (task_id, _, result) in &completed {
                        sess.push(Message {
                            role: Role::System,
                            content: MessageContent::Text(format!(
                                "[async task {task_id} completed]\n{result}"
                            )),
                        });
                    }
                    info!(
                        session = %ctx.session_key,
                        count = completed.len(),
                        "injected async task results"
                    );
                }
            }
        }

        // Dynamic iteration limit based on task complexity.
        // Default: 100 iterations. Complex tools (browser/opencode/exec): up to configured max.
        const BASE_ITERATIONS: usize = 100;
        let configured_complex: usize = self.config.agents.defaults.max_iterations
            .map(|v| v as usize)
            .unwrap_or(500);
        let mut max_iterations = BASE_ITERATIONS;
        let mut iteration = 0usize;

        loop {
            iteration += 1;
            // Check clear_signal mid-loop: clear sessions and abort.
            if self.handle.clear_signal.load(Ordering::SeqCst) {
                self.handle.clear_signal.store(false, Ordering::SeqCst);
                info!(session = %ctx.session_key, "agent_loop: clear_signal, clearing sessions");
                self.sessions.clear();
                self.compaction_state.clear();
                return Ok(AgentReply {
                    text: "[session cleared]".to_string(),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    was_preparse: false,
                });
            }
            // Check abort flag at start of each iteration (allows /abort to
            // interrupt even when tool dispatch is blocking between LLM calls).
            if abort_flag.load(Ordering::SeqCst) {
                abort_flag.store(false, Ordering::SeqCst);
                info!(session = %ctx.session_key, iteration, "agent_loop: aborted by user");
                return Ok(AgentReply {
                    text: "[aborted]".to_string(),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    was_preparse: false,
                });
            }
            if iteration > max_iterations {
                warn!(
                    session = %ctx.session_key,
                    iterations = iteration,
                    "agent_loop: hit max iteration limit, breaking out"
                );
                return Ok(AgentReply {
                    text: crate::i18n::t("agent_max_iterations", crate::i18n::default_lang()).to_owned(),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    was_preparse: false,
                });
            }
            // Apply legacy context pruning (hard clear / soft trim) as fallback.
            if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                apply_context_pruning(sess, pruning_cfg.as_ref());
            }

            // Apply context-budget-aware trimming: trim oldest messages so the
            // total history fits within the model's context window minus reserves
            // for the system prompt, tools, and reply generation.
            if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                apply_context_budget_trim(sess, context_tokens, system_prompt, &tools);
            }

            let messages = {
                let mut raw = self
                    .sessions
                    .get(&ctx.session_key)
                    .cloned()
                    .unwrap_or_default();
                // Replace the last user message with the full version that
                // includes image data (session stores a stripped [image attached]
                // placeholder to avoid bloating persistent storage).
                if ctx.has_images {
                    if let Some(last) = raw.last_mut() {
                        if last.role == Role::User {
                            *last = ctx.user_msg_with_images.clone().unwrap_or(last.clone());
                        }
                    }
                }
                // Strip image data URIs from older messages.
                let stripped = strip_old_images(raw);
                // Repair transcript: ensure all tool_calls have matching tool_results.
                // This fixes orphaned tool_calls from interrupted sessions.
                let repair_result = repair_tool_result_pairing(stripped);

                // Persist any synthetic tool results to session storage
                // so they don't need to be added again on the next turn.
                if !repair_result.synthetic_messages.is_empty() {
                    for synthetic in &repair_result.synthetic_messages {
                        let _ = self.store.db.append_message(
                            &ctx.session_key,
                            &serde_json::to_value(synthetic).unwrap_or_default(),
                        );
                    }
                    // Also update in-memory session cache
                    if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                        sess.extend(repair_result.synthetic_messages.clone());
                    }
                }

                repair_result.messages
            };

            // Resolve thinking budget from agent config or defaults.
            let thinking_budget = {
                let agent_thinking = self
                    .handle
                    .config
                    .model
                    .as_ref()
                    .and_then(|m| m.thinking.as_ref());
                let default_thinking = self.config.agents.defaults.thinking.as_ref();
                let tc = agent_thinking.or(default_thinking);
                tc.and_then(|t| {
                    // Explicit budget_tokens takes precedence.
                    if let Some(budget) = t.budget_tokens {
                        return Some(budget);
                    }
                    // Then try level mapping.
                    if let Some(ref level) = t.level {
                        let b = level.budget_tokens();
                        if b > 0 {
                            return Some(b);
                        }
                    }
                    // Then fall back to enabled bool (medium budget as default).
                    if t.enabled == Some(true) {
                        return Some(10240);
                    }
                    None
                })
            };

            let msg_count = messages.len();
            let approx_tokens: usize = messages.iter().map(msg_tokens).sum();
            self.handle.last_ctx_tokens.store(approx_tokens, std::sync::atomic::Ordering::Relaxed);
            info!(session = %ctx.session_key, msg_count, approx_tokens, model = %model, "LLM call: context size");

            // Context usage awareness: inject hint when usage is high.
            // This goes into the system prompt so the LLM can self-adjust
            // (shorter outputs, avoid re-reading files, etc.).
            let effective_system = if approx_tokens > 0 && context_tokens > 0 {
                let usage_pct = (approx_tokens * 100) / context_tokens;
                if usage_pct >= 90 {
                    format!("{system_prompt}\n\n[Context usage: {usage_pct}% — CRITICAL. \
                        Keep responses very concise. Do not re-read files already in context. \
                        Suggest user start a new session if task is complete.]")
                } else if usage_pct >= 70 {
                    format!("{system_prompt}\n\n[Context usage: {usage_pct}%. \
                        Optimize: keep tool outputs short (use offset/limit for reads, \
                        pipe to head/tail for commands). Avoid re-reading files already in context.]")
                } else {
                    system_prompt.to_owned()
                }
            } else {
                system_prompt.to_owned()
            };

            // Resolve max_tokens with priority: config > built-in defaults > 8192
            let (provider_name, model_id) =
                crate::provider::registry::ProviderRegistry::parse_model(&model);
            let configured_max_tokens = {
                // 1. Agent model config (from handle.config = AgentEntry)
                let from_agent = self.handle.config.model.as_ref().and_then(|m| m.max_tokens);

                // 2. Agent defaults model config (from self.config = RuntimeConfig)
                let from_defaults = self
                    .config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.max_tokens);

                // 3. Provider model definition (from models.providers[].models[])
                let from_provider = self
                    .config
                    .model
                    .models
                    .as_ref()
                    .and_then(|m| m.providers.get(provider_name))
                    .and_then(|p| p.models.as_ref())
                    .and_then(|models| models.iter().find(|m| m.id == model_id))
                    .and_then(|m| m.max_tokens)
                    .map(|v| v as u32);

                from_agent.or(from_defaults).or(from_provider)
            };

            // Only pass max_tokens when explicitly configured.
            // When None, the model/provider decides its own output limit.
            if let Some(configured) = configured_max_tokens {
                info!(
                    session = %ctx.session_key,
                    model = %model,
                    max_tokens = configured,
                    "LLM request max_tokens (from config)"
                );
            }

            // Default temperature 0.6 for tool-calling scenarios (reduces
            // randomness, helps small models preserve digits and paths).
            // For pure chat (no tools), leave as None (provider default, usually 1.0).
            // For thinking/reasoning, leave as None (let provider handle CoT temperature).
            let temperature = if thinking_budget.is_some() || tools.is_empty() {
                None
            } else {
                Some(0.6)
            };

            let req = LlmRequest {
                model: model.to_owned(),
                messages,
                tools: tools.clone(),
                system: Some(effective_system.clone()),
                max_tokens: configured_max_tokens,
                temperature,
                frequency_penalty: self.config.agents.defaults.frequency_penalty,
                thinking_budget,
            };

            // Update live status: LLM call starting.
            if let Ok(mut status) = self.live_status.try_write() {
                status.state = "streaming".to_owned();
            }

            let providers = Arc::clone(&self.providers);
            let mut stream = self.failover.call(req, &providers).await?;
            let mut text_buf = String::new();
            let mut reasoning_buf = String::new();
            let mut tool_calls: Vec<(String, String, Value)> = Vec::new();
            // Track loop detection warnings per tool call id (to inject into result)
            let mut loop_warnings: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            // Streaming throttle: batch small deltas to reduce channel update rate.
            let mut delta_buf = String::new();
            let mut last_delta_flush = std::time::Instant::now();

            while let Some(event) = stream.next().await {
                // Check abort flag.
                if abort_flag.load(Ordering::SeqCst) {
                    abort_flag.store(false, Ordering::SeqCst);
                    return Err(anyhow!("turn aborted"));
                }
                match event? {
                    StreamEvent::TextDelta(delta) => {
                        // Close <think> tag when transitioning from reasoning to text.
                        if thinking_budget.unwrap_or(0) > 0
                            && !reasoning_buf.is_empty()
                            && !text_buf.ends_with("</think>")
                        {
                            text_buf.push_str("</think>");
                            delta_buf.push_str("</think>");
                        }
                        text_buf.push_str(&delta);
                        // Update live status text preview (first ~200 chars).
                        if text_buf.len() <= 250 {
                            if let Ok(mut status) = self.live_status.try_write() {
                                let preview = text_buf
                                    .char_indices()
                                    .nth(200)
                                    .map(|(i, _)| &text_buf[..i])
                                    .unwrap_or(&text_buf);
                                status.text_preview = preview.to_owned();
                            }
                        }
                        // Broadcast incremental delta to SSE subscribers with
                        // debounce: accumulate small deltas and flush when the
                        // buffer reaches a threshold or a pause is detected.
                        // This prevents Feishu/DingTalk card update stutter.
                        delta_buf.push_str(&delta);
                        let now = std::time::Instant::now();
                        let elapsed = now.duration_since(last_delta_flush);
                        if delta_buf.len() >= 80 || elapsed >= std::time::Duration::from_millis(150)
                        {
                            if let Some(ref bus) = self.event_bus {
                                let _ = bus.send(AgentEvent {
                                    session_id: ctx.session_key.clone(),
                                    agent_id: ctx.agent_id.clone(),
                                    delta: std::mem::take(&mut delta_buf),
                                    done: false,
                                });
                            }
                            last_delta_flush = now;
                        }
                    }
                    StreamEvent::ReasoningDelta(delta) => {
                        reasoning_buf.push_str(&delta);
                        // Only emit <think> tags when thinking is explicitly enabled.
                        if thinking_budget.unwrap_or(0) > 0 {
                            if reasoning_buf.len() == delta.len() {
                                // First chunk — open tag.
                                text_buf.push_str("<think>");
                                delta_buf.push_str("<think>");
                            }
                            text_buf.push_str(&delta);
                            delta_buf.push_str(&delta);
                        }
                    }
                    StreamEvent::ToolCall { id, name, input } => {
                        if !id.is_empty() && !name.is_empty() {
                            // New tool call with both id and name — start fresh entry.
                            // Use check_with_params which hashes the full input (OpenClaw-compatible).
                            // This ensures different arguments count as different calls.
                            if let Some(warning_msg) = ctx
                                .loop_detector
                                .check_with_params(&name, &input)
                                .to_result()?
                            {
                                tracing::warn!(tool = %name, params = ?input, "{}", warning_msg);
                                // Store warning to inject into tool result (so LLM sees it)
                                loop_warnings.insert(id.clone(), warning_msg);
                            }
                            tool_calls.push((id, name, input));
                        } else if !id.is_empty() && name.is_empty() {
                            // Streaming tool call: first chunk has id but no name yet
                            tool_calls.push((
                                id,
                                String::new(),
                                serde_json::Value::Object(Default::default()),
                            ));
                        } else if let Some(last) = tool_calls.last_mut() {
                            // Continuation chunk: accumulate name and arguments
                            if !name.is_empty() && last.1.is_empty() {
                                last.1 = name.clone();
                                // Streaming: skip redundant loop check here;
                                // the full check with command content is done
                                // when the complete tool call arrives above.
                            }
                            if !input.is_null()
                                && input != serde_json::Value::Object(Default::default())
                            {
                                // Merge input: if last input is an empty object, replace;
                                // if input is a string (partial args), concatenate.
                                // Do NOT attempt real-time repair here — premature repair
                                // converts the accumulator to an Object, causing subsequent
                                // streaming chunks to be silently dropped (as_str() returns
                                // None for Objects). Repair happens once at finalization.
                                if last.2 == serde_json::Value::Object(Default::default()) {
                                    last.2 = input;
                                } else if let (Some(existing), Some(new_str)) =
                                    (last.2.as_str(), input.as_str())
                                {
                                    let merged = format!("{existing}{new_str}");
                                    last.2 = serde_json::Value::String(merged);
                                }
                            }
                        }
                    }
                    StreamEvent::Done { usage } => {
                        // Update context token count with real usage from LLM if available.
                        if let Some(ref u) = usage {
                            let real_tokens = (u.input + u.output) as usize;
                            self.handle.last_ctx_tokens.store(real_tokens, std::sync::atomic::Ordering::Relaxed);
                            debug!(
                                session = %ctx.session_key,
                                input_tokens = u.input,
                                output_tokens = u.output,
                                "LLM usage (from provider)"
                            );
                        }
                    }
                    StreamEvent::Error(e) => {
                        return Err(anyhow!("LLM stream error: {e}"));
                    }
                }
            }

            // Close unclosed <think> tag if stream ended during reasoning.
            if thinking_budget.unwrap_or(0) > 0
                && !reasoning_buf.is_empty()
                && !text_buf.ends_with("</think>")
            {
                text_buf.push_str("</think>");
                delta_buf.push_str("</think>");
            }

            // Flush any remaining buffered delta.
            if !delta_buf.is_empty() {
                if let Some(ref bus) = self.event_bus {
                    let _ = bus.send(AgentEvent {
                        session_id: ctx.session_key.clone(),
                        agent_id: ctx.agent_id.clone(),
                        delta: delta_buf,
                        done: false,
                    });
                }
            }

            // Strip <think>...</think> tags from accumulated text.
            // Auto-enabled when thinking is not explicitly requested (budget=0 or None),
            // since some models (MiniMax, QwQ) may still emit <think> tags regardless.
            // Can be overridden via agents.defaults.stripThinkTags.
            let pre_strip_len = text_buf.trim().len();
            let thinking_active = thinking_budget.unwrap_or(0) > 0;
            let strip_enabled = self.config.agents.defaults.strip_think_tags.unwrap_or(!thinking_active);
            if strip_enabled {
                let before = text_buf.clone();
                text_buf = crate::provider::openai::strip_think_tags_pub(&text_buf);
                if before != text_buf {
                    tracing::debug!(
                        before_len = before.len(),
                        after_len = text_buf.len(),
                        stripped_bytes = before.len() - text_buf.len(),
                        "strip_think_tags: content changed"
                    );
                }
            }

            // Reasoning models (e.g. kimi-for-coding) may return only reasoning_content
            // with empty content. Use reasoning as the reply text to avoid saving an
            // empty assistant message (which some APIs reject on the next turn).
            tracing::info!(text_len = text_buf.len(), reasoning_len = reasoning_buf.len(), "agent_loop: post-stream buffers");
            if text_buf.trim().is_empty() && !reasoning_buf.trim().is_empty() {
                tracing::info!(reasoning_len = reasoning_buf.len(), "agent_loop: using reasoning as reply text");
                text_buf = reasoning_buf.clone();
            }

            // Finalize streaming tool calls: parse accumulated argument strings.
            for (_id, _name, input) in &mut tool_calls {
                if let serde_json::Value::String(s) = input {
                    // Debug: log the accumulated argument string before parsing
                    tracing::info!(
                        args_len = s.len(),
                        args_start = ?s.chars().take(200).collect::<String>(),
                        args_end = ?s.chars().rev().take(200).collect::<String>().chars().rev().collect::<String>(),
                        "streaming tool call: accumulated args (start and end)"
                    );

                    // First, try direct parse (preserves if valid).
                    // If that fails, fix unescaped backslashes (Windows paths)
                    // before falling through to repair.
                    let parsed = serde_json::from_str::<serde_json::Value>(&s).or_else(|_| {
                        let fixed = crate::agent::tool_call_repair::fix_json_backslashes(&s);
                        serde_json::from_str::<serde_json::Value>(&fixed)
                    });
                    match &parsed {
                        Ok(v) if v.is_object() => {
                            tracing::info!(
                                keys = ?v.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                                "streaming tool call: parsed successfully"
                            );
                            *input = v.clone();
                        }
                        _ => {
                            // Direct parse failed — try to repair malformed JSON
                            // This handles cases where model sends garbage before/after valid JSON
                            match crate::agent::tool_call_repair::try_extract_usable_args(&s) {
                                Some(repair) => {
                                    tracing::warn!(
                                        args_len = s.len(),
                                        repair_kind = ?repair.kind,
                                        leading_prefix_len = repair.leading_prefix.len(),
                                        trailing_suffix_len = repair.trailing_suffix.len(),
                                        "streaming tool call: repaired malformed JSON"
                                    );
                                    *input = repair.args;
                                }
                                None => {
                                    // Repair also failed - check if it's clearly truncated vs
                                    // malformed Truncated:
                                    // starts with valid JSON but ends abruptly
                                    // Malformed: has JSON but syntax is broken
                                    let is_truncated = {
                                        let trimmed = s.trim();
                                        let starts_with_json =
                                            trimmed.starts_with('{') || trimmed.starts_with('[');
                                        let ends_with_complete =
                                            trimmed.ends_with('}') || trimmed.ends_with(']');
                                        starts_with_json && !ends_with_complete
                                    };

                                    tracing::warn!(
                                        args_len = s.len(),
                                        is_truncated = is_truncated,
                                        args_start = ?s.chars().take(100).collect::<String>(),
                                        args_end = ?s.chars().rev().take(50).collect::<String>().chars().rev().collect::<String>(),
                                        "streaming tool call: malformed JSON from model{}",
                                        if is_truncated { " (DETECTED TRUNCATION)" } else { "" }
                                    );

                                    if is_truncated {
                                        // Truncated streaming - the model's output was cut off
                                        // mid-way.
                                        *input = serde_json::json!({
                                            "content": s,
                                            "_parse_error": format!(
                                                "truncated: Your tool call was cut off at {} chars. \
                                                 Try again with shorter content, or split into multiple files.",
                                                s.len()
                                            ),
                                        });
                                    } else {
                                        // Malformed but complete JSON - model made a syntax error
                                        *input = serde_json::json!({
                                            "content": s,
                                            "_parse_error": "Model sent malformed JSON arguments.",
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Drop tool calls with empty names (incomplete streaming)
            tool_calls.retain(|(_, name, _)| !name.is_empty());

            // Rescue tool calls from text output — some small models (qwen3.5:9b)
            // emit tool calls as XML text instead of proper function_call format.
            // Detect <tool_call>/<function=...> patterns and parse them.
            if tool_calls.is_empty() && text_buf.contains("<function=") {
                let re = regex::Regex::new(
                    r#"<function=(\w+)>\s*<parameter=(\w+)>\s*([\s\S]*?)\s*</parameter>\s*</function>"#
                ).unwrap();
                for cap in re.captures_iter(&text_buf) {
                    let name = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    let param = cap.get(2).map(|m| m.as_str()).unwrap_or("");
                    let value = cap.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                    if !name.is_empty() {
                        let input = json!({ param: value });
                        let id = format!("rescued_{name}_{}", tool_calls.len());
                        tracing::info!(name, param, "agent_loop: rescued tool call from text");
                        tool_calls.push((id, name.to_owned(), input));
                    }
                }
                if !tool_calls.is_empty() {
                    // Clear the text since it was a tool call, not a real reply.
                    text_buf.clear();
                }
            }

            // If no tool calls, we have the final assistant reply.
            tracing::info!(
                session = %ctx.session_key,
                tool_call_count = tool_calls.len(),
                text_len = text_buf.len(),
                "agent_loop: stream finished"
            );
            if tool_calls.is_empty() {
                let assistant_msg = Message {
                    role: Role::Assistant,
                    content: MessageContent::Text(text_buf.clone()),
                };
                let _ = self.store.db.append_message(
                    &ctx.session_key,
                    &serde_json::to_value(&assistant_msg).unwrap_or_default(),
                );
                if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                    sess.push(assistant_msg);
                }

                // Broadcast turn-done event to SSE subscribers.
                if let Some(ref bus) = self.event_bus {
                    tracing::debug!(session = %ctx.session_key, "agent_loop: emitting done=true");
                    let _ = bus.send(AgentEvent {
                        session_id: ctx.session_key.clone(),
                        agent_id: ctx.agent_id.clone(),
                        delta: String::new(),
                        done: true,
                    });
                }

                let clean = text_buf.trim().to_uppercase();
                let no_reply = clean.starts_with(NO_REPLY_TOKEN);
                let is_empty = text_buf.trim().is_empty();

                let final_text = if no_reply {
                    String::new()
                } else if is_empty && pre_strip_len > 0 {
                    // Model only produced thinking content; user already saw
                    // it via streaming — return empty without error.
                    String::new()
                } else if is_empty {
                    "[The model returned an empty response. Please try again or rephrase your message.]".to_owned()
                } else {
                    text_buf
                };

                return Ok(AgentReply {
                    text: final_text,
                    is_empty: no_reply && tool_images.is_empty(),
                    tool_calls: None,
                    images: tool_images,
                    files: tool_files,
                    pending_analysis: None,
                    was_preparse: false,
                });
            }

            // Send intermediate text to user immediately (progress feedback).
            // Model often says "好的，我来帮你搜索" before calling tools — send it now
            // instead of waiting for the entire turn to complete.
            let intermediate_enabled = self.config.agents.defaults.intermediate_output.unwrap_or(true);
            if intermediate_enabled && !text_buf.is_empty() && !tool_calls.is_empty() {
                if let Some(ref ntx) = self.notification_tx {
                    let notif_target = if !ctx.chat_id.is_empty() {
                        ctx.chat_id.clone()
                    } else {
                        ctx.peer_id.clone()
                    };
                    let _ = ntx.send(crate::channel::OutboundMessage {
                        target_id: notif_target,
                        is_group: false,
                        text: text_buf.clone(),
                        reply_to: None,
                        images: vec![],
                        files: vec![],
                        channel: Some(ctx.channel.clone()),
                    });
                    tracing::debug!(text_len = text_buf.len(), "agent_loop: sent intermediate text to user");
                }
            }

            // Push assistant message with tool_calls as Parts.
            let mut parts: Vec<crate::provider::ContentPart> = Vec::new();
            if !text_buf.is_empty() {
                parts.push(crate::provider::ContentPart::Text { text: text_buf });
            }
            for (id, name, input) in &tool_calls {
                parts.push(crate::provider::ContentPart::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
            }
            let assistant_msg = Message {
                role: Role::Assistant,
                content: MessageContent::Parts(parts),
            };
            let _ = self.store.db.append_message(
                &ctx.session_key,
                &serde_json::to_value(&assistant_msg).unwrap_or_default(),
            );
            if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                sess.push(assistant_msg);
            }

            // Check if any tool call targets an external (caller-provided) tool.
            // If so, return early with the OAI tool_calls payload — the caller
            // is responsible for executing the tool and continuing the conversation.
            let external_calls: Vec<(String, String, Value)> = tool_calls
                .iter()
                .filter(|(_, name, _)| extra_tools.iter().any(|t| &t.name == name))
                .cloned()
                .collect();

            if !external_calls.is_empty() {
                let oai_tool_calls: Vec<Value> = external_calls
                    .into_iter()
                    .map(|(id, name, input)| {
                        let arguments = if input.is_string() {
                            input.as_str().unwrap_or("{}").to_owned()
                        } else {
                            input.to_string()
                        };
                        json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments
                            }
                        })
                    })
                    .collect();
                return Ok(AgentReply {
                    text: String::new(),
                    is_empty: true,
                    tool_calls: Some(oai_tool_calls),
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    was_preparse: false,
                });
            }

            // Execute each tool and push results.
            for (tool_id, tool_name, tool_input) in tool_calls {
                // Skip tools with parse errors — do not execute, return error directly.
                // This prevents infinite retry loops when model output gets truncated.
                if let Some(parse_error) = tool_input.get("_parse_error").and_then(|v| v.as_str()) {
                    let is_truncated = parse_error.starts_with("truncated:");
                    let err_msg = if is_truncated {
                        "Your tool call was truncated. Try a shorter message or split into multiple steps."
                    } else {
                        "Your tool call contained malformed JSON. Please try again."
                    };
                    warn!(tool = %tool_name, "skipping tool with parse error: {}", parse_error);

                    // Increment parse error counter and check threshold
                    ctx.parse_error_count += 1;
                    if ctx.parse_error_count >= MAX_PARSE_ERRORS {
                        tracing::error!(
                            parse_error_count = ctx.parse_error_count,
                            "Too many consecutive parse errors, aborting turn"
                        );
                        // Record for loop detection
                        ctx.loop_detector.record_result(&serde_json::json!({"error": "too many parse errors"}));
                        // Return error to break the loop
                        return Err(anyhow!(
                            "Turn aborted: {} consecutive tool parse errors. Model output may be corrupted.",
                            ctx.parse_error_count
                        ));
                    }

                    // Record for loop detection so error doesn't count as a "different result"
                    ctx.loop_detector.record_result(&serde_json::json!({"error": err_msg}));

                    // Directly return error to session without executing the tool
                    let tool_msg = Message {
                        role: Role::Tool,
                        content: MessageContent::Parts(vec![
                            crate::provider::ContentPart::ToolResult {
                                tool_use_id: tool_id.clone(),
                                content: format!(r#"{{"error":"{}"}}"#, err_msg),
                                is_error: Some(true),
                            },
                        ]),
                    };
                    let _ = self.store.db.append_message(
                        &ctx.session_key,
                        &serde_json::to_value(&tool_msg).unwrap_or_default(),
                    );
                    if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                        sess.push(tool_msg);
                    }
                    continue;
                }

                debug!(tool = %tool_name, "dispatching tool call");

                // Upgrade iteration limit when complex or multi-step tools are used.
                if matches!(tool_name.as_str(),
                    "web_browser" | "opencode" | "claudecode" | "agent"
                    | "search_content" | "search_file" | "execute_command" | "exec"
                ) {
                    max_iterations = max_iterations.max(configured_complex);
                }

                // Update live status: tool call starting.
                if let Ok(mut status) = self.live_status.try_write() {
                    status.state = "tool_call".to_owned();
                    status.tool_history.push(tool_name.clone());
                }

                self.fire_hook(
                    "before_tool_call",
                    json!({
                        "agent_id": self.handle.id,
                        "tool": tool_name,
                        "input": tool_input,
                    }),
                )
                .await;

                let result = self
                    .dispatch_tool(ctx, &tool_id, &tool_name, tool_input)
                    .await;

                self.fire_hook(
                    "after_tool_call",
                    json!({
                        "agent_id": self.handle.id,
                        "tool": tool_name,
                        "ok": result.is_ok(),
                    }),
                )
                .await;

                let (result_text, result_images) = match result {
                    Ok(v) => {
                        // Reset parse error counter on successful tool execution
                        ctx.parse_error_count = 0;
                        // Record result for progress-aware loop detection.
                        // Same args + different results = making progress, not a loop.
                        ctx.loop_detector.record_result(&v);
                        // Extract images from tool result to avoid passing large
                        // base64 back to LLM. Check "image" (screenshot) and "url" (image gen).
                        let img_data = v.get("image").and_then(|i| i.as_str()).or_else(|| {
                            v.get("url")
                                .and_then(|u| u.as_str())
                                .filter(|u| u.starts_with("data:image/"))
                        });
                        if let Some(img) = img_data {
                            let desc = v
                                .get("revised_prompt")
                                .and_then(|p| p.as_str())
                                .or_else(|| v.get("action").and_then(|a| a.as_str()))
                                .unwrap_or("image generated");
                            (
                                format!(
                                    "{{\"status\":\"image sent to user\",\"description\":\"{desc}\"}}"
                                ),
                                vec![img.to_owned()],
                            )
                        } else if v.is_string() {
                            (v.as_str().unwrap_or("").to_owned(), vec![])
                        } else {
                            // Format structured tool results (exec, read, etc.) for better LLM comprehension
                            (format_tool_result(&v), vec![])
                        }
                    }
                    Err(e) => {
                        warn!(tool = %tool_name, "tool error: {e:#}");
                        // Record error result for loop detection (errors count as results too).
                        ctx.loop_detector
                            .record_result(&serde_json::json!({"error": e.to_string()}));
                        (format!(
                            "{{\"error\":\"{}\",\"_do_not_retry\":true,\"hint\":\"This tool call failed. Do NOT retry the same tool with the same arguments. Try a different approach or inform the user.\"}}",
                            e
                        ), vec![])
                    }
                };

                tool_images.extend(result_images);

                // send_file tool: images go to tool_images, other files to tool_files.
                if tool_name == "send_file" {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&result_text) {
                        if v.get("__send_file").and_then(|b| b.as_bool()).unwrap_or(false) {
                            if let Some(path_str) = v.get("path").and_then(|p| p.as_str()) {
                                let full = std::path::PathBuf::from(path_str);
                                let filename = v.get("filename").and_then(|f| f.as_str()).unwrap_or("file").to_owned();
                                let lower = filename.to_lowercase();
                                let is_image = lower.ends_with(".jpg") || lower.ends_with(".jpeg")
                                    || lower.ends_with(".png") || lower.ends_with(".webp")
                                    || lower.ends_with(".gif");
                                if is_image {
                                    // Send as inline image, not file attachment.
                                    if let Ok(bytes) = std::fs::read(&full) {
                                        use base64::Engine as _;
                                        let mime = if lower.ends_with(".png") { "image/png" }
                                            else if lower.ends_with(".webp") { "image/webp" }
                                            else if lower.ends_with(".gif") { "image/gif" }
                                            else { "image/jpeg" };
                                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                        tool_images.push(format!("data:{mime};base64,{b64}"));
                                        tracing::info!(path = %full.display(), "agent: send_file queued as image");
                                    }
                                } else {
                                    let mime = if lower.ends_with(".xlsx") { "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" }
                                        else if lower.ends_with(".docx") { "application/vnd.openxmlformats-officedocument.wordprocessingml.document" }
                                        else if lower.ends_with(".pptx") { "application/vnd.openxmlformats-officedocument.presentationml.presentation" }
                                        else if lower.ends_with(".pdf") { "application/pdf" }
                                        else if lower.ends_with(".csv") { "text/csv" }
                                        else if lower.ends_with(".mp4") { "video/mp4" }
                                        else if lower.ends_with(".mp3") { "audio/mpeg" }
                                        else if lower.ends_with(".zip") { "application/zip" }
                                        else { "application/octet-stream" };
                                    let full_str = full.to_string_lossy().to_string();
                                    if !tool_files.iter().any(|(_, _, p)| p == &full_str) {
                                        tool_files.push((filename, mime.to_owned(), full_str));
                                        tracing::info!(path = %full.display(), "agent: send_file queued");
                                    }
                                }
                            }
                        }
                    }
                }

                // Collect sendable file attachments from write/exec tool results.
                if matches!(tool_name.as_str(), "write_file" | "write" | "execute_command" | "exec") {
                    let workspace = self.handle.config.workspace.as_deref()
                        .or(self.config.agents.defaults.workspace.as_deref())
                        .map(expand_tilde)
                        .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

                    // Helper: check if a path is a sendable file type and add to tool_files.
                    let mut try_add_file = |path_str: &str| {
                        let lower = path_str.to_lowercase();
                        let sendable_exts = [".xlsx", ".xls", ".docx", ".doc", ".pptx", ".ppt",
                            ".pdf", ".csv", ".mp4", ".mp3", ".zip", ".tar.gz", ".txt", ".json",
                            ".html", ".py", ".md"];
                        if !sendable_exts.iter().any(|ext| lower.ends_with(ext)) { return; }
                        let pb = std::path::PathBuf::from(path_str);
                        let full = if pb.is_absolute() { pb } else { workspace.join(path_str) };
                        if !full.exists() { return; }
                        // Skip very large files (>50MB)
                        if let Ok(meta) = full.metadata() {
                            if meta.len() > 50_000_000 { return; }
                        }
                        let filename = full.file_name().unwrap_or_default().to_string_lossy().to_string();
                        // Avoid duplicates
                        if tool_files.iter().any(|(_, _, p)| p == &full.to_string_lossy().to_string()) { return; }
                        let mime = if lower.ends_with(".xlsx") { "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" }
                            else if lower.ends_with(".docx") { "application/vnd.openxmlformats-officedocument.wordprocessingml.document" }
                            else if lower.ends_with(".pptx") { "application/vnd.openxmlformats-officedocument.presentationml.presentation" }
                            else if lower.ends_with(".pdf") { "application/pdf" }
                            else if lower.ends_with(".csv") { "text/csv" }
                            else if lower.ends_with(".mp4") { "video/mp4" }
                            else if lower.ends_with(".mp3") { "audio/mpeg" }
                            else if lower.ends_with(".zip") { "application/zip" }
                            else { "application/octet-stream" };
                        tool_files.push((filename, mime.to_owned(), full.to_string_lossy().to_string()));
                        tracing::info!(path = %full.display(), "agent: sendable file detected");
                    };

                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&result_text) {
                        // write tool: {"written": true, "path": "xxx.xlsx"}
                        if let Some(path_str) = v.get("path").and_then(|p| p.as_str()) {
                            try_add_file(path_str);
                        }
                        // exec tool: scan stdout for file paths the script may have printed
                        if let Some(stdout) = v.get("stdout").and_then(|s| s.as_str()) {
                            for line in stdout.lines() {
                                let trimmed = line.trim();
                                if trimmed.contains('.') && !trimmed.contains(' ') && trimmed.len() < 256 {
                                    try_add_file(trimmed);
                                }
                            }
                        }
                    }
                }

                // Truncate tool result for session storage (prevent bloat).
                // Current LLM round already has full result via tool dispatch.
                let limits = self
                    .config
                    .ext
                    .tools
                    .as_ref()
                    .and_then(|t| t.session_result_limits.as_ref());
                let max_chars = match tool_name.as_str() {
                    "web_search" => limits.and_then(|l| l.web_search).unwrap_or(2000),
                    "web_fetch" => limits.and_then(|l| l.web_fetch).unwrap_or(5000),
                    "execute_command" | "exec" => limits.and_then(|l| l.exec).unwrap_or(3000),
                    _ => limits.and_then(|l| l.default).unwrap_or(3000),
                };
                let session_text = if result_text.len() > max_chars {
                    let truncated = &result_text[..result_text
                        .char_indices()
                        .nth(max_chars)
                        .map(|(i, _)| i)
                        .unwrap_or(result_text.len())];
                    format!(
                        "{truncated}\n[...truncated, {}/{} chars]",
                        max_chars,
                        result_text.len()
                    )
                } else {
                    result_text.clone()
                };

                // Inject loop detection warning if present (so LLM sees it and can stop)
                let session_text = if let Some(warning) = loop_warnings.get(&tool_id) {
                    format!("[LOOP WARNING] {}\n\n{}", warning, session_text)
                } else {
                    session_text
                };

                let tool_msg = Message {
                    role: Role::Tool,
                    content: MessageContent::Parts(vec![
                        crate::provider::ContentPart::ToolResult {
                            tool_use_id: tool_id.clone(),
                            content: session_text,
                            is_error: Some(false),
                        },
                    ]),
                };
                let _ = self.store.db.append_message(
                    &ctx.session_key,
                    &serde_json::to_value(&tool_msg).unwrap_or_default(),
                );
                if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                    sess.push(tool_msg);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tool dispatch (AGENTS.md §20)
    // -----------------------------------------------------------------------

    async fn dispatch_tool(
        &self,
        ctx: &RunContext,
        _id: &str,
        name: &str,
        args: Value,
    ) -> Result<Value> {
        // 2. Built-in tools (checked before A2A prefix so reserved names are not
        //    hijacked).
        match name {
            // --- Consolidated tools (new unified names) ---
            "memory" => return self.tool_memory_consolidated(ctx, args).await,
            "session" => return self.tool_session_consolidated(ctx, args).await,
            "agent" | "subagents" => return self.tool_agent_consolidated(ctx, args).await,
            "channel" => return self.tool_channel_consolidated(args).await,

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
                    .tool_memory_consolidated(ctx, inject_action(args, "put"))
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
            "agent_spawn" | "sessions_spawn" => {
                return self
                    .tool_agent_consolidated(ctx, inject_action(args, "spawn"))
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
                let workspace = self.handle.config.workspace.as_deref()
                    .or(self.config.agents.defaults.workspace.as_deref())
                    .map(expand_tilde)
                    .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));
                let pb = std::path::PathBuf::from(&path);
                let full = if pb.is_absolute() { pb } else { workspace.join(&path) };

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
                let filename = full.file_name().unwrap_or_default().to_string_lossy().to_string();
                return Ok(json!({
                    "__send_file": true,
                    "path": full.to_string_lossy(),
                    "filename": filename,
                    "size": full.metadata().map(|m| m.len()).unwrap_or(0),
                }));
            }
            "read_file" | "read" => return self.tool_read(args).await,
            "write_file" | "write" => return self.tool_write(args).await,
            "execute_command" | "exec" => return self.tool_exec(ctx, _id, args).await,
            "install_tool" | "tool_install" => return self.tool_install(args).await,
            "list_dir" => return self.tool_list_dir(args).await,
            "search_file" => return self.tool_search_file(args).await,
            "search_content" => return self.tool_search_content(args).await,
            "web_search" => return self.tool_web_search(args).await,
            "web_fetch" => return self.tool_web_fetch(args).await,
            "web_download" => return self.tool_web_download(args).await,
            "web_browser" | "browser" => return self.tool_web_browser(ctx, args).await,
            "computer_use" => return self.tool_computer_use(args).await,
            "image_gen" | "image" => return self.tool_image(args).await,
            "pdf" => return self.tool_pdf(args).await,
            "text_to_voice" | "text_to_speech" | "tts" => return self.tool_tts(args).await,
            "send_message" | "message" => return self.tool_message(args).await,
            "cron" => return self.tool_cron(args).await,
            "gateway" => return self.tool_gateway(args).await,
            "pairing" => return self.tool_pairing(args).await,
            "doc" => return self.tool_doc(args).await,
            "opencode" => return self.tool_opencode(ctx, args).await,
            "claudecode" => return self.tool_claudecode(ctx, args).await,
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

        // 4. Skill tool.
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

    async fn dispatch_a2a(&self, ctx: &RunContext, agent_id: &str, args: Value) -> Result<Value> {
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
                extra_tools: vec![],
                images: vec![],
                files: vec![],
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

        // 2. Fall back to remote A2A gateway (Level 3).
        // Normalize: LLMs sometimes replace _ with - in tool names.
        let normalized_id = agent_id.replace('-', "_");
        if let Some(ext) = self
            .config
            .agents
            .external
            .iter()
            .find(|e| e.id == agent_id || e.id == normalized_id)
        {
            use crate::a2a::client::A2aClient;
            let client = A2aClient::new();
            // Use remote agent ID if configured, otherwise omit (uses remote default).
            let remote_id = ext.remote_agent_id.as_deref().unwrap_or("");
            let reply = client
                .send_task(
                    &ext.url,
                    remote_id,
                    &text,
                    &ctx.session_key,
                    ext.auth_token.as_deref(),
                )
                .await
                .map_err(|e| anyhow!("A2A remote `{agent_id}`: {e}"))?;
            return Ok(Value::String(reply));
        }

        Err(anyhow!(
            "A2A: agent `{agent_id}` not found locally or in external registry"
        ))
    }

    // -----------------------------------------------------------------------
    // Built-in tool implementations
    // -----------------------------------------------------------------------

    async fn tool_memory_search(&self, args: Value) -> Result<Value> {
        let query = args["query"].as_str().unwrap_or("").to_owned();
        let scope = args["scope"].as_str().map(str::to_owned);
        let top_k = args["top_k"].as_u64().unwrap_or(5) as usize;

        let Some(ref mem) = self.memory else {
            return Ok(json!({"results": [], "note": "memory store not available"}));
        };
        let mut store = mem.lock().await;
        let docs = store.search(&query, scope.as_deref(), top_k).await?;
        let results: Vec<Value> = docs
            .into_iter()
            .map(|d| {
                let age = memory_age_label(chrono::Utc::now().timestamp(), d.created_at);
                json!({
                    "id": d.id,
                    "kind": d.kind,
                    "content": d.text,
                    "summary": d.display_text(),
                    "age": age,
                    "importance": d.importance,
                    "access_count": d.access_count,
                })
            })
            .collect();
        Ok(json!({"count": results.len(), "results": results}))
    }

    async fn tool_memory_get(&self, args: Value) -> Result<Value> {
        let id = args["id"].as_str().unwrap_or("").to_owned();
        let Some(ref mem) = self.memory else {
            return Ok(json!({"error": "memory store not available"}));
        };
        let store = mem.lock().await;
        match store.get(&id).await? {
            Some(d) => Ok(json!({"id": d.id, "scope": d.scope, "kind": d.kind, "text": d.text})),
            None => Ok(json!({"error": "not found", "id": id})),
        }
    }

    async fn tool_memory_put(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let text = args["text"].as_str().unwrap_or("").to_owned();
        let scope = args["scope"].as_str().unwrap_or(&ctx.agent_id).to_owned();
        let kind = args["kind"].as_str().unwrap_or("note").to_owned();
        let id = args["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let Some(ref mem) = self.memory else {
            return Ok(json!({"error": "memory store not available"}));
        };
        let mut store = mem.lock().await;
        store
            .add(MemoryDoc {
                id: id.clone(),
                scope: scope.clone(),
                kind: kind.clone(),
                text: text.clone(),
                vector: vec![],
                created_at: 0,
                accessed_at: 0,
                access_count: 0,
                importance: 0.5,
                tier: Default::default(),
                abstract_text: None,
                overview_text: None,
            })
            .await?;
        drop(store);
        // Also index in tantivy BM25 for hybrid search.
        if let Err(e) = self
            .store
            .search
            .index_memory_doc(&id, &scope, &kind, &text)
        {
            tracing::warn!("BM25 index failed for memory_put doc: {e:#}");
        }
        // Only append to MEMORY.md for user-initiated /remember commands,
        // not for automatic memory_put calls by the model.
        if kind != "remember" {
            return Ok(json!({"stored": true, "id": id}));
        }
        let ws_str = self
            .handle
            .config
            .workspace
            .clone()
            .or_else(|| self.config.agents.defaults.workspace.clone())
            .unwrap_or_else(|| "~/.rsclaw/workspace".to_owned());
        let ws = if ws_str.starts_with('~') {
            dirs_next::home_dir().unwrap_or_default().join(&ws_str[2..])
        } else {
            std::path::PathBuf::from(&ws_str)
        };
        let memory_path = ws.join("MEMORY.md");
        let entry = format!(
            "\n## {}\n{}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M"),
            text
        );
        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&memory_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, entry.as_bytes()))
        {
            tracing::warn!("failed to append to MEMORY.md: {e:#}");
        }
        Ok(json!({"stored": true, "id": id}))
    }

    async fn tool_memory_delete(&self, args: Value) -> Result<Value> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow!("memory_delete: `id` required"))?
            .to_owned();
        let Some(ref mem) = self.memory else {
            return Ok(json!({"error": "memory store not available"}));
        };
        mem.lock().await.delete(&id).await?;
        // Also remove from tantivy BM25 index.
        if let Err(e) = self
            .store
            .search
            .delete_document(&id)
            .and_then(|_| self.store.search.commit())
        {
            tracing::warn!("BM25 delete failed for doc {id}: {e:#}");
        }
        Ok(json!({"deleted": true, "id": id}))
    }

    /// Install a tool/runtime via `rsclaw tools install`.
    async fn tool_install(&self, args: Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow!("tool_install: `name` required"))?;

        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "rsclaw".to_owned());

        let mut cmd = tokio::process::Command::new(&exe);
        cmd.args(["tools", "install", name])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let output = cmd.output()
            .await
            .map_err(|e| anyhow!("tool_install: failed to run: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        Ok(json!({
            "name": name,
            "success": output.status.success(),
            "output": if stdout.is_empty() { &stderr } else { &stdout },
        }))
    }

    /// List files and directories in a path (structured alternative to `exec ls`).
    async fn tool_list_dir(&self, args: Value) -> Result<Value> {
        let default_ws = self.handle.config.workspace.as_deref().unwrap_or(".");
        let path_str = args["path"].as_str().unwrap_or(default_ws);
        let path = expand_tilde(path_str);
        let recursive = args["recursive"].as_bool().unwrap_or(false);
        let pattern = args["pattern"].as_str().unwrap_or("*");

        if !path.exists() {
            return Ok(json!({"error": format!("path not found: {}", path.display())}));
        }
        if !path.is_dir() {
            return Ok(json!({"error": format!("not a directory: {}", path.display())}));
        }

        let glob_pattern = if recursive {
            format!("{}/**/{}", path.display(), pattern)
        } else {
            format!("{}/{}", path.display(), pattern)
        };

        let mut entries: Vec<Value> = Vec::new();
        let entries_iter = match glob::glob(&glob_pattern) {
            Ok(iter) => iter,
            Err(e) => return Ok(json!({"error": format!("invalid pattern: {e}")})),
        };
        for entry in entries_iter {
            if entries.len() >= 100 { break; }
            if let Ok(p) = entry {
                let is_dir = p.is_dir();
                let size = if is_dir { 0 } else { p.metadata().map(|m| m.len()).unwrap_or(0) };
                entries.push(json!({
                    "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "path": p.to_string_lossy(),
                    "is_dir": is_dir,
                    "size": size,
                }));
            }
        }

        Ok(json!({
            "path": path.to_string_lossy(),
            "count": entries.len(),
            "entries": entries,
        }))
    }

    /// Search for files by name pattern (structured alternative to `exec find`).
    async fn tool_search_file(&self, args: Value) -> Result<Value> {
        let default_ws = self.handle.config.workspace.as_deref().unwrap_or(".");
        let root = args["path"].as_str().unwrap_or(default_ws);
        let root_path = expand_tilde(root);
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("search_file: `pattern` required"))?;
        let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;

        let glob_pattern = format!("{}/**/{}", root_path.display(), pattern);
        let mut results: Vec<Value> = Vec::new();
        let entries_iter = match glob::glob(&glob_pattern) {
            Ok(iter) => iter,
            Err(e) => return Ok(json!({"error": format!("invalid pattern: {e}")})),
        };
        for entry in entries_iter {
            if results.len() >= max_results { break; }
            if let Ok(p) = entry {
                let size = p.metadata().map(|m| m.len()).unwrap_or(0);
                results.push(json!({
                    "path": p.to_string_lossy(),
                    "size": size,
                    "is_dir": p.is_dir(),
                }));
            }
        }

        Ok(json!({
            "pattern": pattern,
            "root": root_path.to_string_lossy(),
            "count": results.len(),
            "results": results,
        }))
    }

    /// Search file contents by pattern (structured alternative to `exec grep`).
    ///
    /// Cross-platform: uses `grep -rn` on Unix, `Select-String` on Windows.
    async fn tool_search_content(&self, args: Value) -> Result<Value> {
        let default_ws = self.handle.config.workspace.as_deref().unwrap_or(".");
        let root = args["path"].as_str().unwrap_or(default_ws);
        let root_path = expand_tilde(root);
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("search_content: `pattern` required"))?;
        let include = args["include"].as_str();
        let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
        let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;

        #[cfg(not(target_os = "windows"))]
        let output = {
            let mut cmd = tokio::process::Command::new("grep");
            cmd.arg("-rn");
            if ignore_case { cmd.arg("-i"); }
            if let Some(inc) = include {
                cmd.arg("--include").arg(inc);
            }
            cmd.arg("--").arg(pattern).arg(root_path.to_str().unwrap_or("."));
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::null());
            tokio::time::timeout(Duration::from_secs(15), cmd.output())
                .await
                .map_err(|_| anyhow!("search_content: timed out"))?
                .map_err(|e| anyhow!("search_content: {e}"))?
        };

        #[cfg(target_os = "windows")]
        let output = {
            // PowerShell Select-String is the Windows equivalent of grep -rn.
            let mut ps_args = vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
            ];
            let inc_filter = include
                .map(|i| format!(" -Include '{}'", i.replace('\'', "''")))
                .unwrap_or_default();
            let case_flag = if ignore_case { "" } else { " -CaseSensitive" };
            // Use TAB as separator to avoid conflicts with drive-letter colons in Windows paths.
            // Escape single quotes in all interpolated values to prevent PowerShell injection.
            let safe_path = root_path.display().to_string().replace('\'', "''");
            let safe_pattern = pattern.replace('\'', "''");
            let ps_cmd = format!(
                "Get-ChildItem -Path '{safe_path}' -Recurse{inc_filter} -File | Select-String -Pattern '{safe_pattern}'{case_flag} | Select-Object -First {max_results} | ForEach-Object {{ \"$($_.Path)\t$($_.LineNumber)\t$($_.Line)\" }}"
            );
            ps_args.push(ps_cmd);
            let mut cmd = tokio::process::Command::new("powershell");
            cmd.args(&ps_args);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::null());
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }
            tokio::time::timeout(Duration::from_secs(15), cmd.output())
                .await
                .map_err(|_| anyhow!("search_content: timed out"))?
                .map_err(|e| anyhow!("search_content: {e}"))?
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut matches: Vec<Value> = Vec::new();
        // Windows uses TAB separator, Unix uses colon.
        let sep = if cfg!(target_os = "windows") { '\t' } else { ':' };
        for line in stdout.lines() {
            if matches.len() >= max_results { break; }
            // Parse: file<sep>line<sep>content
            // On Unix with colons: handle drive-less paths (no ambiguity).
            // On Windows with TABs: no ambiguity with path colons.
            let parts: Vec<&str> = line.splitn(3, sep).collect();
            if parts.len() == 3 {
                matches.push(json!({
                    "file": parts[0],
                    "line": parts[1].parse::<u64>().unwrap_or(0),
                    "content": parts[2].chars().take(200).collect::<String>(),
                }));
            }
        }

        Ok(json!({
            "pattern": pattern,
            "root": root_path.to_string_lossy(),
            "count": matches.len(),
            "matches": matches,
        }))
    }

    async fn tool_read(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .or_else(|| args["file_path"].as_str())
            .or_else(|| args["filename"].as_str())
            .or_else(|| args["file"].as_str())
            .ok_or_else(|| anyhow!("read: `path` required"))?;
        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

        // Normalize path separators for Windows
        let path_normalized = path.replace('/', std::path::MAIN_SEPARATOR.to_string().as_str());
        let path_buf = std::path::PathBuf::from(&path_normalized);
        let full = if path_buf.is_absolute() {
            path_buf
        } else {
            workspace.join(&path_normalized)
        };

        // Safety: block reading sensitive files
        let safety_enabled = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.safety)
            .unwrap_or(false);
        if safety_enabled {
            check_read_safety(path, &full)?;
        }

        let lower = path.to_lowercase();
        // Binary file types: extract text instead of raw read
        if lower.ends_with(".pdf") {
            let pdf_bytes = tokio::fs::read(&full)
                .await
                .map_err(|e| anyhow!("read `{}`: {e}", full.display()))?;
            let content = match crate::agent::doc::safe_extract_pdf_from_mem(&pdf_bytes) {
                Ok(text) => text,
                Err(e) => {
                    // Fallback to pdftotext CLI
                    tracing::warn!("pdf-extract failed ({e}), trying pdftotext CLI");
                    let output = tokio::process::Command::new("pdftotext")
                        .args([full.to_str().unwrap_or(""), "-"])
                        .output()
                        .await
                        .map_err(|e2| {
                            anyhow!(
                                "read `{}`: pdf extraction failed: {e}, pdftotext: {e2}",
                                full.display()
                            )
                        })?;
                    if !output.status.success() {
                        anyhow::bail!("read `{}`: pdf extraction failed: {e}", full.display());
                    }
                    String::from_utf8_lossy(&output.stdout).to_string()
                }
            };
            return Ok(json!({"content": content, "path": path}));
        }
        if lower.ends_with(".docx") || lower.ends_with(".xlsx") || lower.ends_with(".pptx") {
            let bytes = tokio::fs::read(&full)
                .await
                .map_err(|e| anyhow!("read `{}`: {e}", full.display()))?;
            if let Some(text) = crate::channel::extract_office_text(path, &bytes) {
                return Ok(json!({"content": text, "path": path}));
            }
            anyhow::bail!("read `{}`: failed to extract office text", full.display());
        }

        let content = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| anyhow!("read `{}`: {e}", full.display()))?;
        Ok(json!({"content": content, "path": path}))
    }

    async fn tool_write(&self, args: Value) -> Result<Value> {
        // Check if this is a malformed JSON case from streaming
        if let Some(parse_error) = args.get("_parse_error").and_then(|v| v.as_str()) {
            tracing::warn!("tool_write: received malformed JSON from model");
            let is_truncated = parse_error.starts_with("truncated:");
            return Ok(json!({
                "error": if is_truncated { "Your tool call was truncated by the API." } else { "Your tool call contained malformed JSON arguments." },
                "details": parse_error,
                "hint": if is_truncated {
                    "The API truncated your response. Split into multiple smaller writes (under 3500 chars each)."
                } else {
                    "Ensure all quotes/backslashes are escaped and JSON is complete."
                }
            }));
        }

        // Handle various parameter names LLMs might use.
        let path = args["path"]
            .as_str()
            .or_else(|| args["file_path"].as_str())
            .or_else(|| args["filename"].as_str())
            .or_else(|| args["file"].as_str())
            .or_else(|| args.as_str());
        let content = args["content"].as_str();

        if path.is_none() || path.map(|p| p.is_empty()).unwrap_or(true) {
            let has_content = content.map(|c| !c.is_empty()).unwrap_or(false);
            tracing::warn!(has_content, "tool_write: missing path parameter");
            return Ok(json!({
                "error": "Missing 'path' parameter. The write tool requires BOTH 'path' and 'content'.",
                "hint": "Retry with: {\"path\": \"file.py\", \"content\": \"...\"}"
            }));
        }

        if content.is_none() {
            tracing::warn!("tool_write: missing content parameter");
            return Ok(json!({
                "error": "Missing 'content' parameter.",
                "hint": "Provide a 'content' parameter with the text to write."
            }));
        }

        let path = path.unwrap().to_owned();
        let content = content.unwrap().to_owned();
        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

        // Normalize path separators for Windows
        let path_normalized = path.replace('/', std::path::MAIN_SEPARATOR.to_string().as_str());
        let path_buf = std::path::PathBuf::from(&path_normalized);
        let full = if path_buf.is_absolute() {
            path_buf
        } else {
            workspace.join(&path_normalized)
        };

        // Safety: block sensitive paths (only when tools.exec.safety = true)
        let safety_enabled = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.safety)
            .unwrap_or(false);
        if safety_enabled {
            check_write_safety(&path, &full, &content)?;
        }

        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full, &content)
            .await
            .map_err(|e| anyhow!("write `{}`: {e}", full.display()))?;
        Ok(json!({"written": true, "path": path, "bytes": content.len()}))
    }

    // -----------------------------------------------------------------------
    // Compaction (AGENTS.md §15)
    // -----------------------------------------------------------------------

    /// Summarise the session history via LLM when the total character count
    /// approaches `reserveTokensFloor` (approximated as floor * 4 chars/token,
    /// default 100 000 chars).
    ///
    /// **Layered mode** (default): keeps the last N user-assistant pairs
    /// verbatim and only summarises the older portion, so recent context is
    /// never lost.  Falls back to Default/Safeguard when configured.
    async fn compact_if_needed(&mut self, session_key: &str, model: &str) {
        self.compact_inner(session_key, model, false).await;
    }

    /// Force compaction regardless of threshold (used by /compact).
    async fn compact_force(&mut self, session_key: &str, model: &str) {
        self.compact_inner(session_key, model, true).await;
    }

    async fn compact_inner(&mut self, session_key: &str, model: &str, force: bool) {
        use crate::config::schema::CompactionMode;

        // Use configured compaction settings, or sensible defaults.
        let cfg = self.config.agents.defaults.compaction.clone()
            .unwrap_or_default();

        // Multi-condition compaction trigger: token threshold OR turn count OR time
        // elapsed. Whichever fires first.
        // Default threshold: 80% of configured context window, minimum 16000.
        let context_tokens = self.config.agents.defaults.context_tokens.unwrap_or(64_000) as usize;
        let default_threshold = (context_tokens * 4 / 5).max(16_000);
        let token_threshold = cfg
            .reserve_tokens_floor
            .map(|t| t as usize)
            .unwrap_or(default_threshold);
        let max_turns: u32 = 60;
        let max_elapsed_secs: u64 = 30 * 60; // 30 minutes

        let total_tokens: usize = self
            .sessions
            .get(session_key)
            .map(|msgs| msgs.iter().map(msg_tokens).sum())
            .unwrap_or(0);

        let (last_compaction, turns) = self
            .compaction_state
            .get(session_key)
            .copied()
            .unwrap_or((std::time::Instant::now(), 0));

        let token_trigger = total_tokens > token_threshold;
        let turn_trigger = turns >= max_turns;
        let time_trigger = last_compaction.elapsed().as_secs() >= max_elapsed_secs
            && total_tokens > token_threshold / 2;

        debug!(
            session = session_key,
            total_tokens,
            token_threshold,
            turns,
            token_trigger,
            turn_trigger,
            time_trigger,
            force,
            "compaction check"
        );

        if !force && !token_trigger && !turn_trigger && !time_trigger {
            // Increment turn counter only.
            self.compaction_state
                .entry(session_key.to_owned())
                .and_modify(|(_, t)| *t += 1)
                .or_insert((std::time::Instant::now(), 1));
            return;
        }

        let trigger_reason = if token_trigger {
            "tokens"
        } else if turn_trigger {
            "turns"
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
            let old_portion = &msgs[..split_idx];
            let recent = msgs[split_idx..].to_vec();
            if old_portion.is_empty() {
                // Not enough history to compact -- skip.
                return;
            }
            (msgs_to_text(old_portion), recent)
        } else {
            let msgs = self.sessions.get(session_key).cloned().unwrap_or_default();
            (msgs_to_text(&msgs), vec![])
        };

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
                            };
                            let _ = guard.add(doc).await;
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
            let _ = self.store.db.delete_session(session_key);
            for msg in sess.iter() {
                let val = serde_json::to_value(msg).unwrap_or_default();
                let _ = self.store.db.append_message(session_key, &val);
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
    fn msgs_to_text_static(msgs: &[Message], max_total_tokens: usize) -> String {
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
    async fn compact_single(&mut self, model: &str, history: &str) -> Option<String> {
        let req = LlmRequest {
            model: model.to_owned(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(format!(
                    "Summarize the following conversation into these sections:\n\n\
                     1. **Task & Intent**: What the user wants to accomplish\n\
                     2. **Key Data**: Exact values that MUST be preserved verbatim \
                        (file paths, URLs, cron expressions, config values, IDs, \
                        variable names, port numbers, credentials)\n\
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
    async fn extract_key_facts(&mut self, model: &str, history: &str) -> Option<String> {
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
                     '- '). Include: names, user IDs, chat IDs, important decisions, file \
                     paths, URLs, preferences, and action items. Be concise. Skip ephemeral \
                     chit-chat.\n\n{input}"
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
    // JSONL transcript (AGENTS.md §20 step 11)
    // -----------------------------------------------------------------------

    /// Append user + assistant messages to `~/.rsclaw/transcripts/<key>.jsonl`.
    async fn append_transcript(&self, session_key: &str, user_text: &str, assistant_text: &str) {
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

    async fn tool_exec(&self, ctx: &RunContext, tool_call_id: &str, args: Value) -> Result<Value> {
        tracing::info!(
            session_key = %ctx.session_key,
            tool_call_id = %tool_call_id,
            args = ?args,
            "tool_exec: called"
        );

        // Check if this is a poll request for an existing task
        if let Some(task_id) = args["task_id"].as_str() {
            tracing::info!(
                session_key = %ctx.session_key,
                task_id = %task_id,
                "tool_exec: polling existing task"
            );
            return self.exec_poll_task(task_id).await;
        }

        // Get wait parameter - if true, execute synchronously and block until completion
        let wait = args["wait"].as_bool().unwrap_or(false);
        tracing::debug!(
            session_key = %ctx.session_key,
            wait = wait,
            "tool_exec: wait parameter"
        );

        // Accept both "command" (rsclaw native) and "cmd"+"args" (preparse/openclaw format).
        let command = if let Some(cmd) = args["command"].as_str() {
            cmd.to_owned()
        } else if let Some(cmd) = args["cmd"].as_str() {
            // Reconstruct command string from cmd + args array.
            // Quote args containing spaces/special chars to preserve paths
            // like "C:/Program Files/chrome/chrome.exe".
            let cmd_args = args["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| {
                            if s.contains(' ') || s.contains('\"') || s.contains('\'') {
                                format!("\"{}\"", s.replace('\"', "\\\""))
                            } else {
                                s.to_owned()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            if cmd_args.is_empty() {
                cmd.to_owned()
            } else {
                format!("{cmd} {cmd_args}")
            }
        } else {
            bail!("exec: `command` required (or use `task_id` to poll an existing task)");
        };
        let command = command.as_str();

        // Safety check (only when tools.exec.safety = true)
        let safety_enabled = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.safety)
            .unwrap_or(false);

        if safety_enabled {
            let preparse = crate::agent::preparse::PreParseEngine::load_with_safety(true);
            match preparse.check_exec_safety(command) {
                crate::agent::preparse::SafetyCheck::Allow => {}
                crate::agent::preparse::SafetyCheck::Deny(reason) => {
                    bail!("[blocked] {reason}");
                }
                crate::agent::preparse::SafetyCheck::Confirm(reason) => {
                    bail!("[needs confirmation] {reason}. Command: {command}");
                }
            }
        }

        // Always run via shell to support pipes, redirects, &&, etc.
        let (shell, shell_args) = if cfg!(target_os = "windows") {
            // PowerShell: better compatibility, supports pipes, redirects, && via -Command
            ("powershell", vec!["-NoProfile", "-Command"])
        } else {
            ("sh", vec!["-c"])
        };

        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

        // Interpreter file scan + sandbox (only when safety enabled)
        if safety_enabled {
            let cmd_tokens: Vec<&str> = command.split_whitespace().collect();
            const INTERPRETERS: &[&str] = &[
                "bash",
                "sh",
                "zsh",
                "fish",
                "dash",
                "csh",
                "tcsh",
                "python",
                "python3",
                "python2",
                "ruby",
                "perl",
                "node",
                "bun",
                "deno",
                "powershell",
                "pwsh",
            ];
            if let Some(first) = cmd_tokens.first() {
                if INTERPRETERS
                    .iter()
                    .any(|i| first.ends_with(i) || *first == *i)
                {
                    if let Some(file_arg) = cmd_tokens.get(1) {
                        let file_path = std::path::Path::new(file_arg);
                        let resolved = if file_path.is_absolute() {
                            file_path.to_path_buf()
                        } else {
                            workspace.join(file_path)
                        };
                        check_file_content_safety(&resolved)?;
                    }
                }
            }

            // Sandbox: restrict file access to workspace only.
            let ws_canon = if workspace.exists() {
                std::fs::canonicalize(&workspace).unwrap_or_else(|_| workspace.clone())
            } else {
                workspace.clone()
            };
            for token in command.split_whitespace() {
                let is_abs = std::path::Path::new(token).is_absolute();
                if is_abs || token.contains("..") {
                    let resolved = if is_abs {
                        std::path::PathBuf::from(token)
                    } else {
                        workspace.join(token)
                    };
                    let canon = if resolved.exists() {
                        std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone())
                    } else {
                        resolved.clone()
                    };
                    if !canon.starts_with(&ws_canon) {
                        bail!("[sandbox] access denied: path `{token}` is outside workspace");
                    }
                }
            }
        }

        tracing::info!(cwd = %workspace.display(), command = %command, "exec: spawning in background");

        // Timeout for exec commands (default 1800s = 30 min, matching openclaw).
        let timeout_secs = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.timeout_seconds)
            .unwrap_or(1800);

        // Generate unique task ID
        let task_id = format!(
            "exec-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().to_string()[..8].to_owned()
        );

        // If wait=true, execute synchronously and return result immediately
        if wait {
            tracing::info!(cwd = %workspace.display(), command = %command, task_id = %task_id, "exec: executing synchronously (wait=true)");

            let mut cmd = tokio::process::Command::new(shell);
        // Prepend ~/.rsclaw/tools/* to PATH so locally installed tools are found first.
        let tools_base = crate::config::loader::base_dir().join("tools");
        if tools_base.exists() {
            let mut extra_paths = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&tools_base) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        // Add the dir itself, bin/, and node_modules/.bin/ subdirectories.
                        extra_paths.push(p.join("node_modules").join(".bin"));
                        extra_paths.push(p.join("bin"));
                        extra_paths.push(p.clone());
                    }
                }
            }
            if !extra_paths.is_empty() {
                let sys_path = std::env::var("PATH").unwrap_or_default();
                let mut all: Vec<String> = extra_paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
                all.push(sys_path);
                cmd.env("PATH", all.join(if cfg!(windows) { ";" } else { ":" }));
            }
        }
        cmd.args(&shell_args)
            .arg(command)
            .current_dir(&workspace)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            cmd.output()
        )
        .await
        .map_err(|_| {
            tracing::warn!(command = %command, timeout_secs, "exec: timed out");
            anyhow!(
                "Command timed out after {} seconds. If this command is expected to take longer, re-run with a higher timeout via config.",
                timeout_secs
            )
        })?
        .map_err(|e| anyhow!("exec `{command}`: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::info!(cwd = %workspace.display(), command = %command, exit_code = ?output.status.code(), stdout_len = stdout.len(), stderr_len = stderr.len(), "exec: done");

        return Ok(json!({
            "task_id": task_id,
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
        }));
        }

        // Otherwise, spawn in background without blocking
        tracing::info!(
            cwd = %workspace.display(),
            command = %command,
            task_id = %task_id,
            timeout_secs = timeout_secs,
            shell = shell,
            "exec: spawning background task"
        );

        // Clone for the spawned task
        let exec_pool = Arc::clone(&self.exec_pool);
        let session_key_spawn = ctx.session_key.clone();
        let tool_call_id_owned = tool_call_id.to_owned();
        let command_owned = command.to_owned();
        let workspace_clone = workspace.clone();
        let shell_owned = shell.to_owned();
        let shell_args_owned = shell_args.clone();
        let task_id_clone = task_id.clone();

        tracing::debug!(
            task_id = %task_id_clone,
            session_key = %session_key_spawn,
            tool_call_id = %tool_call_id_owned,
            "exec: cloned variables for background spawn"
        );

        // Spawn the command in background without blocking
        tokio::spawn(async move {
            tracing::info!(
                task_id = %task_id_clone,
                command = %command_owned,
                "exec_background: spawned task started executing"
            );
            let started_at = std::time::Instant::now();

            let exit_code;
            let stdout;
            let stderr;

            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x08000000;
                match tokio::time::timeout(
                    std::time::Duration::from_secs(timeout_secs),
                    tokio::process::Command::new(&shell_owned)
                        .args(&shell_args_owned)
                        .arg(&command_owned)
                        .current_dir(&workspace_clone)
                        .creation_flags(CREATE_NO_WINDOW)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .kill_on_drop(true)
                        .output()
                )
                .await
                {
                    Ok(Ok(output)) => {
                        exit_code = output.status.code();
                        stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                        stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                        tracing::info!(
                            task_id = %task_id_clone,
                            command = %command_owned,
                            exit_code = ?exit_code,
                            stdout_len = stdout.len(),
                            stderr_len = stderr.len(),
                            "exec background completed"
                        );
                    }
                    Ok(Err(e)) => {
                        exit_code = None;
                        stdout = String::new();
                        stderr = format!("spawn error: {}", e);
                        tracing::error!(task_id = %task_id_clone, "exec background spawn failed: {}", e);
                    }
                    Err(_) => {
                        exit_code = None;
                        stdout = String::new();
                        stderr = format!("timed out after {} seconds", timeout_secs);
                        tracing::warn!(task_id = %task_id_clone, timeout_secs, "exec background timed out");
                    }
                };
            }
            #[cfg(not(windows))]
            {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(timeout_secs),
                    tokio::process::Command::new(&shell_owned)
                        .args(&shell_args_owned)
                        .arg(&command_owned)
                        .current_dir(&workspace_clone)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .kill_on_drop(true)
                        .output()
                )
                .await
                {
                    Ok(Ok(output)) => {
                        exit_code = output.status.code();
                        stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                        stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                        tracing::info!(
                            task_id = %task_id_clone,
                            command = %command_owned,
                            exit_code = ?exit_code,
                            stdout_len = stdout.len(),
                            stderr_len = stderr.len(),
                            "exec background completed"
                        );
                    }
                    Ok(Err(e)) => {
                        exit_code = None;
                        stdout = String::new();
                        stderr = format!("spawn error: {}", e);
                        tracing::error!(task_id = %task_id_clone, "exec background spawn failed: {}", e);
                    }
                    Err(_) => {
                        exit_code = None;
                        stdout = String::new();
                        stderr = format!("timed out after {} seconds", timeout_secs);
                        tracing::warn!(task_id = %task_id_clone, timeout_secs, "exec background timed out");
                    }
                };
            }

            let completed_at = std::time::Instant::now();
            let elapsed_ms = (completed_at - started_at).as_millis();

            // Store the result for collection on next turn (or poll)
            tracing::info!(
                task_id = %task_id_clone,
                session_key = %session_key_spawn,
                tool_call_id = %tool_call_id_owned,
                exit_code = ?exit_code,
                elapsed_ms = elapsed_ms,
                "exec_background: storing result in pending queue"
            );
            let exec_result = super::exec_pool::ExecResult {
                task_id: task_id_clone.clone(),
                tool_call_id: tool_call_id_owned,
                command: command_owned,
                exit_code,
                stdout,
                stderr,
                started_at,
                completed_at,
            };
            exec_pool.add_pending_for_session(session_key_spawn, exec_result).await;
        });

        tracing::info!(
            task_id = %task_id,
            session_key = %ctx.session_key,
            "exec: background task spawned, returning task_id"
        );
        let hint = format!("To get result: call exec with the task_id to poll. Example: exec {{\"task_id\": \"{}\"}}", task_id);
        Ok(json!({
            "exec_task_id": task_id,
            "status": "running",
            "message": "Command started in background. Use exec with task_id to poll status.",
            "hint": hint
        }))
    }

    /// Poll the status of a background exec task.
    /// This is NON-BLOCKING - just checks if result is available.
    async fn exec_poll_task(&self, task_id: &str) -> Result<Value> {
        tracing::info!(task_id = %task_id, "exec_poll: checking task status");

        // Check if task is still running
        let is_running = self.exec_pool.is_running(task_id).await;
        tracing::debug!(task_id = %task_id, is_running = is_running, "exec_poll: is_running check result");

        if is_running {
            tracing::info!(task_id = %task_id, "exec_poll: task still running, returning running status");
            return Ok(json!({
                "task_id": task_id,
                "status": "running",
                "message": "Task is still running. Poll again later to get the result when ready."
            }));
        }

        // Try to collect the result
        tracing::debug!(task_id = %task_id, "exec_poll: task not running, attempting to collect result");
        let result = self.exec_pool.try_collect_by_task(task_id).await;

        match result {
            Some(res) => {
                tracing::info!(
                    task_id = %task_id,
                    tool_call_id = %res.tool_call_id,
                    command = %res.command,
                    exit_code = ?res.exit_code,
                    stdout_len = res.stdout.len(),
                    stderr_len = res.stderr.len(),
                    elapsed_ms = (res.completed_at - res.started_at).as_millis(),
                    "exec_poll: task completed, returning result"
                );
                Ok(json!({
                    "task_id": task_id,
                    "status": "completed",
                    "exit_code": res.exit_code,
                    "stdout": res.stdout,
                    "stderr": res.stderr,
                }))
            }
            None => {
                // Task not found - might have been collected already or never existed
                Ok(json!({
                    "task_id": task_id,
                    "status": "not_found",
                    "message": "Task not found. It may have already been collected (check previous messages) or never existed."
                }))
            }
        }
    }

    /// Build a full system prompt for a sub-agent by combining the shared base
    /// (date, platform, safety rules, agent loop guidance) with the role-specific
    /// description provided by the main agent.
    fn build_subagent_system_prompt(&self, role_desc: &str) -> String {
        let base_parts = build_base_system_prompt(&self.config.raw);
        let mut prompt = base_parts.join("\n\n");
        prompt.push_str("\n\n## Your Role\n");
        prompt.push_str(role_desc);
        prompt.push_str(
            "\n\n## Sub-Agent Guidelines\n\
             - You are a sub-agent working on a delegated task. Focus on the task and return results.\n\
             - Use the tools available to you. If a tool is not in your toolset, find an alternative.\n\
             - Be concise in your reply — the main agent will relay your output to the user.\n\
             - If the task is unclear or impossible, explain why instead of looping.",
        );
        prompt
    }

    async fn tool_agent_spawn(&self, args: Value) -> Result<Value> {
        let spawner = self
            .spawner
            .as_ref()
            .ok_or_else(|| anyhow!("agent_spawn: spawner not available"))?;

        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_spawn: `id` required"))?
            .to_owned();
        let model = args["model"].as_str()
            .filter(|s| !s.is_empty() && *s != "default")
            .unwrap_or(&self.resolve_model_name())
            .to_owned();
        let system = args["system"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_spawn: `system` required"))?
            .to_owned();
        let toolset_str = args["toolset"]
            .as_str()
            .unwrap_or("standard")
            .to_owned();
        let channels: Option<Vec<String>> = args["channels"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_owned())).collect());

        use crate::config::schema::{AgentEntry, ModelConfig};

        let entry = AgentEntry {
            id: id.clone(),
            default: Some(false),
            workspace: Some(crate::config::loader::path_to_forward_slash(
                &crate::config::loader::base_dir().join(format!("workspace-{id}")),
            )),
            model: Some(ModelConfig {
                primary: Some(model),
                fallbacks: None,
                image: None,
                image_fallbacks: None,
                thinking: None,
                tools_enabled: None,
                toolset: Some(toolset_str.clone()),
                tools: None,
                context_tokens: None,
                max_tokens: None,
            }),
            lane: None,
            lane_concurrency: None,
            group_chat: None,
            channels: channels.clone(),
            name: None,
            agent_dir: None,
            system: None,
            commands: None,
            allowed_commands: None,
            opencode: None,
            claudecode: None,
        };

        spawner.spawn_agent(entry.clone())?;

        // Write full system prompt (base + role) as SOUL.md in the new agent's workspace.
        let ws_path = crate::config::loader::base_dir().join(format!("workspace-{id}"));
        if let Err(e) = tokio::fs::create_dir_all(&ws_path).await {
            warn!("agent_spawn: failed to create workspace for {id}: {e:#}");
        }
        let soul_path = ws_path.join("SOUL.md");
        let full_prompt = self.build_subagent_system_prompt(&system);
        if let Err(e) = tokio::fs::write(&soul_path, format!("# Agent: {id}\n\n{full_prompt}\n")).await {
            warn!("agent_spawn: failed to write SOUL.md for {id}: {e:#}");
        }

        // Persist to config file by default (user-created agents survive restart).
        // Pass persistent=false only for temporary task-delegation agents.
        let persistent = args["persistent"].as_bool().unwrap_or(true);
        if persistent {
            if let Err(e) = persist_agent_to_config(&entry).await {
                warn!("agent_spawn: failed to persist to config: {e:#}");
            }
        }

        let needs_restart = persistent && channels.is_some();
        Ok(json!({
            "spawned": id,
            "model": args["model"],
            "persistent": persistent,
            "channels": channels,
            "needs_restart": needs_restart,
            "status": if needs_restart { "saved — restart gateway to bind channels" } else { "ready" }
        }))
    }

    /// One-shot task agent: spawn -> send message -> return immediately.
    ///
    /// The task runs in the background. When the sub-agent completes, the
    /// result is stored in `pending_task_results` and injected into the
    /// main agent's session on the next turn. This ensures the main agent
    /// is NEVER blocked by sub-agent work.
    async fn tool_agent_task(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let spawner = self
            .spawner
            .as_ref()
            .ok_or_else(|| anyhow!("agent_task: spawner not available"))?;

        let model = args["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.resolve_model_name())
            .to_owned();

        let system = args["system"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_task: `system` required"))?
            .to_owned();

        let message = args["message"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_task: `message` required"))?
            .to_owned();

        let toolset_str = args["toolset"]
            .as_str()
            .unwrap_or("standard")
            .to_owned();

        let short_id = &uuid::Uuid::new_v4().to_string()[..8];
        let id = format!("task-{short_id}");
        let base = crate::config::loader::base_dir();
        let parent_ws = self.handle.config.workspace
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| base.join("workspace"));
        let ws_path = parent_ws.join(format!("task-{short_id}"));
        use crate::config::schema::{AgentEntry, ModelConfig};
        let entry = AgentEntry {
            id: id.clone(),
            default: Some(false),
            workspace: Some(crate::config::loader::path_to_forward_slash(&ws_path)),
            model: Some(ModelConfig {
                primary: Some(model),
                fallbacks: None,
                image: None,
                image_fallbacks: None,
                thinking: None,
                tools_enabled: None,
                toolset: Some(toolset_str.clone()),
                tools: None,
                context_tokens: None,
                max_tokens: None,
            }),
            lane: None,
            lane_concurrency: None,
            group_chat: None,
            channels: None,
            name: None,
            agent_dir: None,
            system: None,
            commands: None,
            allowed_commands: None,
            opencode: None,
            claudecode: None,
        };

        spawner.spawn_agent(entry)?;

        // Write full system prompt (base + role) as SOUL.md.
        if let Err(e) = tokio::fs::create_dir_all(&ws_path).await {
            warn!("agent_task: failed to create workspace for {id}: {e:#}");
        }
        let full_prompt = self.build_subagent_system_prompt(&system);
        if let Err(e) = tokio::fs::write(ws_path.join("SOUL.md"), format!("# Agent: {id}\n\n{full_prompt}\n")).await {
            warn!("agent_task: failed to write SOUL.md for {id}: {e:#}");
        }

        // Send message to the task agent.
        let registry = self
            .agents
            .as_ref()
            .ok_or_else(|| anyhow!("agent_task: agent registry not available"))?;
        let target = registry.get(&id)?;
        let task_session = format!("{}:task:{short_id}", ctx.session_key);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentReply>();
        let msg = AgentMessage {
            session_key: task_session,
            text: message.clone(),
            channel: format!("task:{}", ctx.agent_id),
            peer_id: ctx.agent_id.clone(),
            chat_id: String::new(),
            reply_tx,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
        };
        target.tx.send(msg).await.map_err(|_| anyhow!("agent_task: agent inbox closed"))?;

        // Spawn background worker to wait for reply and store result.
        // Main agent returns IMMEDIATELY — never blocked.
        let pending = Arc::clone(&self.pending_task_results);
        let session_key = ctx.session_key.clone();
        let task_id = id.clone();
        let agents = self.agents.as_ref().map(Arc::clone);
        let timeout_secs = self
            .config
            .agents
            .defaults
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS as u32) as u64;
        let task_timeout = timeout_secs.min(300); // up to 5 min for background tasks

        tokio::spawn(async move {
            let result_text = match tokio::time::timeout(
                Duration::from_secs(task_timeout),
                reply_rx,
            ).await {
                Ok(Ok(reply)) => reply.text,
                Ok(Err(_)) => "[task agent channel closed unexpectedly]".to_owned(),
                Err(_) => format!("[task {task_id} timed out after {task_timeout}s]"),
            };

            // Store result for main agent to pick up.
            if let Ok(mut guard) = pending.lock() {
                guard.push((task_id.clone(), session_key, result_text));
            }

            // Cleanup: remove agent from registry, delete workspace.
            if let Some(reg) = agents {
                reg.remove_handle(&task_id);
            }
            let _ = tokio::fs::remove_dir_all(&ws_path).await;
            info!(task = %task_id, "async task agent completed and cleaned up");
        });

        Ok(json!({
            "task_id": id,
            "status": "dispatched",
            "toolset": toolset_str,
            "message": message,
            "note": "Task is running in the background. Results will be available on your next turn. You can continue with other work."
        }))
    }

    /// Send a message to a persistent (spawned) sub-agent. Non-blocking.
    async fn tool_agent_send(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let target_id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_send: `id` required"))?
            .to_owned();
        let message = args["message"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_send: `message` required"))?
            .to_owned();

        let registry = self
            .agents
            .as_ref()
            .ok_or_else(|| anyhow!("agent_send: agent registry not available"))?;
        let target = registry.get(&target_id)?;

        let short_id = &uuid::Uuid::new_v4().to_string()[..8];
        let send_session = format!("{}:send:{short_id}", ctx.session_key);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentReply>();
        let msg = AgentMessage {
            session_key: send_session,
            text: message.clone(),
            channel: format!("send:{}", ctx.agent_id),
            peer_id: ctx.agent_id.clone(),
            chat_id: String::new(),
            reply_tx,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
        };
        target.tx.send(msg).await.map_err(|_| anyhow!("agent_send: agent '{target_id}' inbox closed"))?;

        // Background: wait for reply and store in pending results.
        let pending = Arc::clone(&self.pending_task_results);
        let session_key = ctx.session_key.clone();
        let timeout_secs = self
            .config
            .agents
            .defaults
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS as u32) as u64;
        let send_timeout = timeout_secs.min(300);
        let send_id = format!("send-{target_id}-{short_id}");
        let send_id_bg = send_id.clone();
        let target_id_bg = target_id.clone();

        tokio::spawn(async move {
            let result_text = match tokio::time::timeout(
                Duration::from_secs(send_timeout),
                reply_rx,
            ).await {
                Ok(Ok(reply)) => reply.text,
                Ok(Err(_)) => format!("[agent {target_id_bg} channel closed]"),
                Err(_) => format!("[agent {target_id_bg} timed out after {send_timeout}s]"),
            };
            if let Ok(mut guard) = pending.lock() {
                guard.push((send_id_bg, session_key, result_text));
            }
        });

        Ok(json!({
            "send_id": send_id,
            "target": target_id,
            "status": "sent",
            "note": "Message sent to agent. Reply will be available on your next turn."
        }))
    }

    async fn tool_agent_list(&self) -> Result<Value> {
        let agents = match &self.agents {
            Some(reg) => reg
                .all()
                .iter()
                .map(|h| {
                    json!({
                        "id": h.id,
                        "model": h.config.model.as_ref()
                            .and_then(|m| m.primary.as_deref())
                            .unwrap_or("unknown"),
                    })
                })
                .collect::<Vec<_>>(),
            None => vec![],
        };
        Ok(json!({"agents": agents}))
    }

    // -----------------------------------------------------------------------
    // Web tools
    // -----------------------------------------------------------------------

    async fn tool_web_search(&self, args: Value) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow!("web_search: `query` required"))?;

        // Read config
        let ws_cfg = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.web_search.as_ref());
        let limit = args["limit"]
            .as_u64()
            .unwrap_or_else(|| ws_cfg.and_then(|c| c.max_results).unwrap_or(5) as u64)
            as usize;
        let provider_raw = args["provider"].as_str().unwrap_or("");
        // Normalize: "auto-detect", "auto", "default" -> empty (trigger auto-detect
        // logic)
        let provider = match provider_raw {
            "auto-detect" | "auto" | "default" | "none" => "",
            other => other,
        };

        // Resolve API keys: config first, then env vars
        let resolve_key = |cfg_key: Option<&crate::config::schema::SecretOrString>,
                           env_name: &str|
         -> Option<String> {
            cfg_key
                .and_then(|k| k.resolve_early())
                .or_else(|| std::env::var(env_name).ok())
                .filter(|k| !k.is_empty())
        };
        let brave_key = resolve_key(
            ws_cfg.and_then(|c| c.brave_api_key.as_ref()),
            "BRAVE_API_KEY",
        );
        let google_key = resolve_key(
            ws_cfg.and_then(|c| c.google_api_key.as_ref()),
            "GOOGLE_SEARCH_API_KEY",
        );
        let google_cx = ws_cfg
            .and_then(|c| c.google_cx.clone())
            .or_else(|| std::env::var("GOOGLE_SEARCH_CX").ok());
        let bing_key = resolve_key(ws_cfg.and_then(|c| c.bing_api_key.as_ref()), "BING_API_KEY");
        let serper_key = resolve_key(
            ws_cfg.and_then(|c| c.serper_api_key.as_ref()),
            "SERPER_API_KEY",
        );

        // Auto-detect provider: explicit arg > config default > keyed provider >
        // DuckDuckGo
        let chosen = if !provider.is_empty() {
            provider.to_owned()
        } else if let Some(default) = ws_cfg.and_then(|c| c.provider.as_deref()) {
            default.to_owned()
        } else if serper_key.is_some() {
            "serper".to_owned()
        } else if brave_key.is_some() {
            "brave".to_owned()
        } else if google_key.is_some() && google_cx.is_some() {
            "google".to_owned()
        } else if bing_key.is_some() {
            "bing".to_owned()
        } else {
            "bing-free".to_owned()
        };

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .timeout(Duration::from_secs(15))
            .build()?;

        let mut results: Vec<Value> = match chosen.as_str() {
            "duckduckgo-free" => {
                let base = search_engine_url("duckduckgo");
                let url = format!(
                    "{}?q={}",
                    if base.is_empty() {
                        "https://html.duckduckgo.com/html/"
                    } else {
                        base
                    },
                    urlencoding::encode(query)
                );
                let html = client.get(&url).send().await?.text().await?;
                parse_ddg_results(&html, limit)
            }
            "google" => {
                let (key, cx) = match (google_key, google_cx) {
                    (Some(k), Some(c)) => (k, c),
                    _ => {
                        // Missing google credentials, fall back to DuckDuckGo
                        tracing::warn!(
                            "web_search: google credentials incomplete, falling back to DuckDuckGo"
                        );
                        let url = format!(
                            "{}?q={}",
                            {
                                let b = search_engine_url("duckduckgo");
                                if b.is_empty() {
                                    "https://html.duckduckgo.com/html/"
                                } else {
                                    b
                                }
                            },
                            urlencoding::encode(query)
                        );
                        let html = client.get(&url).send().await?.text().await?;
                        return Ok(
                            json!({"results": parse_ddg_results(&html, limit), "provider": "duckduckgo (fallback)"}),
                        );
                    }
                };
                let base = search_engine_url("google");
                let resp: Value = client
                    .get(if base.is_empty() {
                        "https://www.googleapis.com/customsearch/v1"
                    } else {
                        base
                    })
                    .query(&[
                        ("key", key.as_str()),
                        ("cx", cx.as_str()),
                        ("q", query),
                        ("num", &limit.min(10).to_string()),
                    ])
                    .send()
                    .await?
                    .json()
                    .await?;
                resp["items"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit)
                            .map(|item| {
                                json!({
                                    "title": item["title"].as_str().unwrap_or(""),
                                    "url": item["link"].as_str().unwrap_or(""),
                                    "snippet": item["snippet"].as_str().unwrap_or("")
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "bing" => {
                let key = bing_key.ok_or_else(|| anyhow!("web_search: bing API key not set (config tools.webSearch.bingApiKey or env BING_API_KEY)"))?;
                let base = search_engine_url("bing");
                let resp: Value = client
                    .get(if base.is_empty() {
                        "https://api.bing.microsoft.com/v7.0/search"
                    } else {
                        base
                    })
                    .query(&[("q", query), ("count", &limit.to_string())])
                    .header("Ocp-Apim-Subscription-Key", &key)
                    .send()
                    .await?
                    .json()
                    .await?;
                resp["webPages"]["value"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit)
                            .map(|item| {
                                json!({
                                    "title": item["name"].as_str().unwrap_or(""),
                                    "url": item["url"].as_str().unwrap_or(""),
                                    "snippet": item["snippet"].as_str().unwrap_or("")
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "brave" => {
                let key = brave_key.ok_or_else(|| anyhow!("web_search: brave API key not set (config tools.webSearch.braveApiKey or env BRAVE_API_KEY)"))?;
                let base = search_engine_url("brave");
                let resp: Value = client
                    .get(if base.is_empty() {
                        "https://api.search.brave.com/res/v1/web/search"
                    } else {
                        base
                    })
                    .query(&[("q", query), ("count", &limit.to_string())])
                    .header("X-Subscription-Token", &key)
                    .send()
                    .await?
                    .json()
                    .await?;
                resp["web"]["results"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit)
                            .map(|item| {
                                json!({
                                    "title": item["title"].as_str().unwrap_or(""),
                                    "url": item["url"].as_str().unwrap_or(""),
                                    "snippet": item["description"].as_str().unwrap_or("")
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "serper" => {
                let key = serper_key.ok_or_else(|| anyhow!("web_search: serper API key not set (config tools.webSearch.serperApiKey or env SERPER_API_KEY)"))?;
                let resp: Value = client
                    .post("https://google.serper.dev/search")
                    .header("X-API-KEY", &key)
                    .header("Content-Type", "application/json")
                    .json(&json!({ "q": query, "num": limit.min(10) }))
                    .send()
                    .await?
                    .json()
                    .await?;
                resp["organic"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit)
                            .map(|item| {
                                json!({
                                    "title": item["title"].as_str().unwrap_or(""),
                                    "url": item["link"].as_str().unwrap_or(""),
                                    "snippet": item["snippet"].as_str().unwrap_or("")
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            // Free HTML scraping providers (no API key needed)
            "bing-free" => {
                let lang = self
                    .config
                    .raw
                    .gateway
                    .as_ref()
                    .and_then(|g| g.language.as_deref())
                    .unwrap_or("");
                let is_zh = lang.to_lowercase().starts_with("zh")
                    || lang.to_lowercase().starts_with("chinese");
                let bing_host = if is_zh { "cn.bing.com" } else { "www.bing.com" };
                let mkt = lang_to_bing_mkt(lang);
                let mkt_param = if mkt.is_empty() {
                    String::new()
                } else {
                    format!("&mkt={mkt}&setlang={}", &mkt[..2])
                };
                let url = format!(
                    "https://{bing_host}/search?q={}&count={limit}{mkt_param}",
                    urlencoding::encode(query)
                );
                let html = client
                    .get(&url)
                    .header(
                        "User-Agent",
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
                    )
                    .send()
                    .await?
                    .text()
                    .await?;
                parse_bing_html_results(&html, limit)
            }
            "baidu-free" => {
                let url = format!(
                    "https://www.baidu.com/s?wd={}&rn={limit}",
                    urlencoding::encode(query)
                );
                let html = client
                    .get(&url)
                    .header(
                        "User-Agent",
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
                    )
                    .send()
                    .await?
                    .text()
                    .await?;
                parse_baidu_results(&html, limit)
            }
            "sogou-free" => {
                let url = format!(
                    "https://www.sogou.com/web?query={}&num={limit}",
                    urlencoding::encode(query)
                );
                let html = client
                    .get(&url)
                    .header(
                        "User-Agent",
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
                    )
                    .send()
                    .await?
                    .text()
                    .await?;
                parse_sogou_results(&html, limit)
            }
            other => return Err(anyhow!("web_search: unknown provider `{other}`")),
        };

        // Fallback: if DDG returned empty (captcha), try bing-free
        if results.is_empty() && chosen == "duckduckgo-free" {
            tracing::warn!("web_search: DuckDuckGo returned 0 results, falling back to bing-free");
            let lang = self
                .config
                .raw
                .gateway
                .as_ref()
                .and_then(|g| g.language.as_deref())
                .unwrap_or("");
            let is_zh = lang.to_lowercase().starts_with("zh")
                || lang.to_lowercase().starts_with("chinese");
            let bing_host = if is_zh { "cn.bing.com" } else { "www.bing.com" };
            let mkt = lang_to_bing_mkt(lang);
            let mkt_param = if mkt.is_empty() {
                String::new()
            } else {
                format!("&mkt={mkt}&setlang={}", &mkt[..2])
            };
            let url = format!(
                "https://{bing_host}/search?q={}&count={limit}{mkt_param}",
                urlencoding::encode(query)
            );
            let html = client
                .get(&url)
                .header(
                    "User-Agent",
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
                )
                .send()
                .await?
                .text()
                .await?;
            let fallback = parse_bing_html_results(&html, limit);
            if !fallback.is_empty() {
                results = fallback;
            }
        }

        // --- Multi-provider parallel merge (free providers only) ---
        // When no API key is configured (free scraping mode), run 2 providers
        // concurrently for better coverage. Provider pair selected by language:
        //   zh → random 2 from [bing-free, baidu, sogou, 360]
        //   other → bing-free + duckduckgo
        let free_providers = ["duckduckgo-free", "bing-free", "baidu-free", "sogou-free"];
        let is_free_mode = free_providers.contains(&chosen.as_str());
        if is_free_mode {
            let lang = self.config.raw.gateway.as_ref()
                .and_then(|g| g.language.as_deref())
                .unwrap_or("");
            let is_zh = lang.starts_with("zh")
                || std::env::var("LANG").unwrap_or_default().to_lowercase().contains("zh");

            let pair: [&str; 2] = if is_zh {
                // Chinese: random 2 from 4 free Chinese-friendly providers.
                let mut pool = vec!["bing-free", "baidu-free", "sogou-free"];
                use rand::seq::SliceRandom;
                pool.shuffle(&mut rand::rng());
                [pool[0], pool[1]]
            } else {
                ["bing-free", "duckduckgo-free"]
            };

            // Run both in parallel.
            let (r1, r2) = tokio::join!(
                self.search_provider(pair[0], query, limit, &client),
                self.search_provider(pair[1], query, limit, &client),
            );

            // Merge both into results, dedup by URL.
            results.clear();
            let mut seen_urls = std::collections::HashSet::new();
            for batch in [r1, r2] {
                if let Ok(items) = batch {
                    for r in items {
                        if let Some(url) = r["url"].as_str() {
                            if seen_urls.insert(url.to_owned()) {
                                results.push(r);
                            }
                        }
                    }
                }
            }
        }

        // --- Browser fallback: when all free providers are blocked by CAPTCHA ---
        if results.is_empty() && is_free_mode {
            info!("web_search: all free providers returned empty, trying browser fallback");
            match self.browser_search(query, limit).await {
                Ok(browser_results) if !browser_results.is_empty() => {
                    info!(count = browser_results.len(), "web_search: browser fallback succeeded");
                    results = browser_results;
                }
                Ok(_) => warn!("web_search: browser fallback also returned empty"),
                Err(e) => warn!("web_search: browser fallback failed: {e:#}"),
            }
        }

        // --- Auto-fetch top 3 results for deeper content ---
        let fetch_count = results.len().min(3);
        let fetch_urls: Vec<String> = results.iter()
            .take(fetch_count)
            .filter_map(|r| r["url"].as_str().map(String::from))
            .collect();

        if !fetch_urls.is_empty() {
            let fetch_client = reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());

            // Fetch all URLs concurrently.
            let fetches = fetch_urls.iter().map(|url| {
                let client = fetch_client.clone();
                let url = url.clone();
                async move {
                    let resp = client.get(&url).send().await.ok()?;
                    let html = resp.text().await.ok()?;
                    let content_type = "text/html"; // assume HTML
                    let md = if content_type.contains("text/html") {
                        htmd::convert(&html).unwrap_or_else(|_| strip_html(&html))
                    } else {
                        html
                    };
                    // Truncate to 2000 chars.
                    let truncated = truncate_chars(&md, 2000);
                    Some((url, truncated))
                }
            });
            let fetched: Vec<Option<(String, String)>> = futures::future::join_all(fetches).await;

            // Attach content to matching results.
            for (url, content) in fetched.into_iter().flatten() {
                for r in results.iter_mut() {
                    if r["url"].as_str() == Some(url.as_str()) {
                        r["content"] = json!(content);
                        break;
                    }
                }
            }
        }

        // If still empty after all attempts, add a hint about API keys.
        if results.is_empty() && is_free_mode {
            let i18n_lang = crate::i18n::default_lang();
            return Ok(json!({
                "results": [],
                "provider": chosen,
                "error": crate::i18n::t("search_captcha_blocked", i18n_lang)
            }));
        }

        Ok(json!({ "results": results, "provider": chosen }))
    }

    /// Helper: run a free scraping search provider and return results.
    async fn search_provider(
        &self,
        provider: &str,
        query: &str,
        limit: usize,
        client: &reqwest::Client,
    ) -> Result<Vec<Value>> {
        let lang = self.config.raw.gateway.as_ref()
            .and_then(|g| g.language.as_deref())
            .unwrap_or("");
        let is_zh = lang.to_lowercase().starts_with("zh")
            || lang.to_lowercase().starts_with("chinese");
        let (html, results) = match provider {
            "bing-free" => {
                let bing_host = if is_zh { "cn.bing.com" } else { "www.bing.com" };
                let mkt = lang_to_bing_mkt(lang);
                let mkt_param = if mkt.is_empty() {
                    String::new()
                } else {
                    format!("&mkt={mkt}&setlang={}", &mkt[..2])
                };
                let url = format!(
                    "https://{bing_host}/search?q={}&count={limit}{mkt_param}",
                    urlencoding::encode(query)
                );
                let html = client
                    .get(&url)
                    .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
                    .send().await?.text().await?;
                let r = parse_bing_html_results(&html, limit);
                (html, r)
            }
            "duckduckgo-free" => {
                let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding::encode(query));
                let html = client.get(&url).send().await?.text().await?;
                let r = parse_ddg_results(&html, limit);
                (html, r)
            }
            "baidu-free" => {
                let url = format!("https://www.baidu.com/s?wd={}&rn={limit}", urlencoding::encode(query));
                let html = client.get(&url)
                    .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
                    .send().await?.text().await?;
                let r = parse_baidu_results(&html, limit);
                (html, r)
            }
            "sogou-free" => {
                let url = format!("https://www.sogou.com/web?query={}", urlencoding::encode(query));
                let html = client.get(&url)
                    .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
                    .send().await?.text().await?;
                let r = parse_sogou_results(&html, limit);
                (html, r)
            }
            _ => return Ok(vec![]),
        };

        if results.is_empty() && is_captcha_page(&html) {
            warn!(provider, "web_search: CAPTCHA detected, provider may be rate-limited");
        }

        Ok(results)
    }

    async fn tool_web_fetch(&self, args: Value) -> Result<Value> {
        use moka::future::Cache;
        use std::sync::LazyLock;

        /// LRU cache: URL → (title, markdown). 15 min TTL, ~50 MB.
        static FETCH_CACHE: LazyLock<Cache<String, (String, String)>> = LazyLock::new(|| {
            Cache::builder()
                .max_capacity(500)
                .time_to_live(Duration::from_secs(15 * 60))
                .build()
        });

        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow!("web_fetch: `url` required"))?;
        let prompt = args.get("prompt").and_then(|v| v.as_str());

        let max_length = self.config.ext.tools.as_ref()
            .and_then(|t| t.web_fetch.as_ref())
            .and_then(|f| f.max_length)
            .unwrap_or(100_000);
        let user_agent = self.config.ext.tools.as_ref()
            .and_then(|t| t.web_fetch.as_ref())
            .and_then(|f| f.user_agent.clone())
            .unwrap_or_else(|| "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_owned());

        // Upgrade http → https.
        let fetch_url = if url.starts_with("http://") {
            url.replacen("http://", "https://", 1)
        } else {
            url.to_owned()
        };

        // Check cache.
        if let Some((cached_title, cached_md)) = FETCH_CACHE.get(&fetch_url).await {
            let text = truncate_chars(&cached_md, max_length);
            let text = self.maybe_summarize(&text, prompt).await;
            return Ok(json!({
                "url": url,
                "title": cached_title,
                "text": text,
                "length": text.len(),
            }));
        }

        // Build HTTP client with same-host-only redirect policy.
        let original_host = reqwest::Url::parse(&fetch_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_owned()));
        let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > 10 {
                return attempt.error(anyhow!("too many redirects"));
            }
            // Allow same-host (ignoring www. prefix).
            let new_host = attempt.url().host_str().unwrap_or("");
            let strip_www = |h: &str| h.strip_prefix("www.").unwrap_or(h).to_owned();
            let orig = original_host.as_deref().map(strip_www).unwrap_or_default();
            if strip_www(new_host) == orig {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });

        let client = reqwest::Client::builder()
            .user_agent(&user_agent)
            .timeout(Duration::from_secs(30))
            .redirect(redirect_policy)
            .build()?;

        let response = client.get(&fetch_url).send().await?;

        // Cross-host redirect: report to agent, let it decide.
        if response.status().is_redirection() {
            if let Some(loc) = response.headers().get("location").and_then(|v| v.to_str().ok()) {
                return Ok(json!({
                    "url": url,
                    "redirect": loc,
                    "text": format!("Redirected to different host: {loc}. Fetch that URL if needed."),
                }));
            }
        }

        // Enforce 10 MB content-length limit.
        if let Some(len) = response.content_length() {
            if len > 10 * 1024 * 1024 {
                bail!("web_fetch: content too large ({} bytes, max 10MB)", len);
            }
        }

        let content_type = response.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let html = response.text().await?;

        let title = extract_html_title(&html);

        // Convert HTML → Markdown (htmd, Turndown-inspired).
        let markdown = if content_type.contains("text/html") {
            htmd::convert(&html).unwrap_or_else(|_| strip_html(&html))
        } else {
            html.clone()
        };

        // Detect SPA (large HTML but almost no text) → fallback to browser.
        let plain_len = strip_html(&html).trim().len();
        let is_spa = content_type.contains("text/html") && plain_len < 200 && html.len() > 10_000;

        let (final_title, final_md) = if is_spa {
            // Try browser fallback for JS-rendered pages.
            match self.browser_get_article(&fetch_url).await {
                Ok((t, md)) if !md.is_empty() => (t, md),
                _ => (title.clone(), markdown.clone()),
            }
        } else {
            (title.clone(), markdown.clone())
        };

        // Cache the result.
        FETCH_CACHE.insert(fetch_url, (final_title.clone(), final_md.clone())).await;

        let text = truncate_chars(&final_md, max_length);
        let text = self.maybe_summarize(&text, prompt).await;

        Ok(json!({
            "url": url,
            "title": final_title,
            "text": text,
            "length": text.len(),
        }))
    }

    /// Use web_browser to fetch JS-rendered page content via get_article.
    async fn browser_get_article(&self, url: &str) -> Result<(String, String)> {
        let mut browser = self.browser.lock().await;

        // Ensure browser session exists.
        if browser.is_none() {
            let wb_cfg = self.config.ext.tools.as_ref()
                .and_then(|t| t.web_browser.as_ref());
            let chrome_path = wb_cfg
                .and_then(|b| b.chrome_path.clone())
                .or_else(|| crate::agent::runtime::detect_chrome())
                .ok_or_else(|| anyhow!("Chrome not found for SPA fallback"))?;
            crate::browser::can_launch_chrome()?;
            let headed = wb_cfg.and_then(|b| b.headed).unwrap_or_else(has_display);
            let profile = wb_cfg.and_then(|b| b.profile.clone());
            *browser = Some(crate::browser::BrowserSession::start(&chrome_path, headed, profile.as_deref()).await?);
        }

        let bs = browser.as_mut().unwrap();
        bs.execute("open", &json!({"url": url})).await?;
        let article = bs.execute("get_article", &json!({})).await?;

        let title = article["title"].as_str().unwrap_or("").to_owned();
        let text = article["text"].as_str().unwrap_or("").to_owned();
        Ok((title, text))
    }

    /// Browser-based search fallback: open a search engine in the browser,
    /// extract results from the rendered page. Bypasses CAPTCHA since it uses
    /// a real browser session.
    async fn browser_search(&self, query: &str, limit: usize) -> Result<Vec<Value>> {
        let mut browser = self.browser.lock().await;

        if browser.is_none() {
            let wb_cfg = self.config.ext.tools.as_ref()
                .and_then(|t| t.web_browser.as_ref());
            let chrome_path = wb_cfg
                .and_then(|b| b.chrome_path.clone())
                .or_else(|| crate::agent::runtime::detect_chrome())
                .ok_or_else(|| anyhow!("Chrome not found for browser search fallback"))?;
            crate::browser::can_launch_chrome()?;
            let headed = wb_cfg.and_then(|b| b.headed).unwrap_or_else(has_display);
            let profile = wb_cfg.and_then(|b| b.profile.clone());
            *browser = Some(crate::browser::BrowserSession::start(&chrome_path, headed, profile.as_deref()).await?);
        }

        let bs = browser.as_mut().unwrap();

        // Try multiple search engines, auto-switch on CAPTCHA/empty results.
        let lang = self.config.raw.gateway.as_ref()
            .and_then(|g| g.language.as_deref())
            .unwrap_or("");
        let is_zh = lang.to_lowercase().starts_with("zh")
            || lang.to_lowercase().starts_with("chinese");

        // Engine list: (name, url_template, result_css, snippet_css)
        let q = urlencoding::encode(query);
        let engines: Vec<(&str, String, &str, &str)> = if is_zh {
            vec![
                ("baidu", format!("https://www.baidu.com/s?wd={q}"), ".result.c-container", "p, .c-abstract"),
                ("sogou", format!("https://www.sogou.com/web?query={q}"), ".vrwrap, .rb", "p, .ft"),
                ("bing", format!("https://cn.bing.com/search?q={q}"), ".b_algo", "p"),
                ("google", format!("https://www.google.com/search?q={q}"), "div.g", "span.st, div[data-sncf]"),
            ]
        } else {
            vec![
                ("google", format!("https://www.google.com/search?q={q}"), "div.g", "span.st, div[data-sncf]"),
                ("bing", format!("https://www.bing.com/search?q={q}"), ".b_algo", "p"),
                ("duckduckgo", format!("https://html.duckduckgo.com/html/?q={q}"), ".result", ".result__snippet"),
            ]
        };

        for (name, url, result_selector, snippet_selector) in &engines {
            info!(engine = name, "browser_search: trying");
            if let Err(e) = bs.execute("open", &json!({"url": url})).await {
                warn!(engine = name, "browser_search: open failed: {e}");
                continue;
            }
            let _ = bs.execute("wait", &json!({"target": "element", "value": *result_selector, "timeout": 8})).await;

            // Check for CAPTCHA: look for common challenge indicators
            let captcha_js = r#"(function(){
                var t = document.body ? document.body.innerText.toLowerCase() : '';
                var hasCaptcha = t.includes('captcha') || t.includes('验证') || t.includes('robot')
                    || t.includes('unusual traffic') || t.includes('人机验证')
                    || document.querySelector('iframe[src*="captcha"]') !== null
                    || document.querySelector('#captcha, .captcha, .g-recaptcha') !== null;
                return hasCaptcha ? 'captcha' : 'ok';
            })()"#;
            let check = bs.execute("evaluate", &json!({"js": captcha_js})).await;
            if let Ok(ref v) = check {
                let status = v["result"].as_str().unwrap_or("");
                if status == "captcha" {
                    warn!(engine = name, "browser_search: CAPTCHA detected, trying next engine");
                    continue;
                }
            }

            // Extract results
            let js = format!(r#"(function(){{
                var results = [];
                var items = document.querySelectorAll('{result_selector}');
                for (var i = 0; i < Math.min(items.length, {limit}); i++) {{
                    var a = items[i].querySelector('a');
                    var p = items[i].querySelector('{snippet_selector}');
                    if (a && a.href && !a.href.startsWith('javascript:')) {{
                        results.push({{
                            title: a.innerText || '',
                            url: a.href || '',
                            snippet: p ? p.innerText || '' : ''
                        }});
                    }}
                }}
                return JSON.stringify(results);
            }})()"#);

            let result = bs.execute("evaluate", &json!({"js": js})).await?;
            let result_str = result["result"].as_str().unwrap_or("[]");
            let parsed: Vec<Value> = serde_json::from_str(
                if result_str.starts_with('[') { result_str } else { "[]" }
            ).unwrap_or_default();

            if !parsed.is_empty() {
                info!(engine = name, count = parsed.len(), "browser_search: got results");
                return Ok(parsed);
            }
            warn!(engine = name, "browser_search: no results, trying next engine");
        }

        Ok(vec![])
    }

    /// If summaryModel is configured and a prompt is provided, summarize
    /// the content with a secondary model. Otherwise return content as-is.
    async fn maybe_summarize(&self, content: &str, prompt: Option<&str>) -> String {
        let summary_model = self.config.ext.tools.as_ref()
            .and_then(|t| t.web_fetch.as_ref())
            .and_then(|f| f.summary_model.clone());

        let (Some(model_str), Some(prompt)) = (summary_model, prompt) else {
            return content.to_owned();
        };

        // Resolve provider/model and call directly (bypass failover for simplicity).
        let (provider_name, model_id) = self.providers.resolve_model(&model_str);

        let provider = match self.providers.get(provider_name) {
            Ok(p) => p,
            Err(e) => {
                warn!("web_fetch: provider '{provider_name}' not available: {e}");
                return content.to_owned();
            }
        };

        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text(format!(
                "Web page content:\n---\n{content}\n---\n\n{prompt}\n\n\
                 Provide a concise response based on the content above."
            )),
        }];

        let req = crate::provider::LlmRequest {
            model: model_id.to_owned(),
            messages,
            tools: vec![],
            system: None,
            max_tokens: Some(2000),
            temperature: None,
            frequency_penalty: None,
            thinking_budget: None,
        };

        match provider.stream(req).await {
            Ok(mut stream) => {
                let mut buf = String::new();
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(StreamEvent::TextDelta(d)) => buf.push_str(&d),
                        Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                if buf.is_empty() { content.to_owned() } else { buf }
            }
            Err(e) => {
                warn!("web_fetch summary model failed: {e:#}");
                content.to_owned()
            }
        }
    }

    async fn tool_web_download(&self, args: Value) -> Result<Value> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow!("web_download: `url` required"))?;
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("web_download: `path` required"))?;

        // Resolve path: always under workspace/downloads.
        // Strip common prefixes that models hallucinate (~/Downloads/, ~/,  /workspace/).
        let mut cleaned = path_str
            .trim_start_matches("~/Downloads/")
            .trim_start_matches("~/downloads/")
            .trim_start_matches("~/")
            .trim_start_matches("/workspace/")
            .trim_start_matches("/");
        if cleaned.is_empty() {
            cleaned = "download";
        }
        let workspace = self.handle.config.workspace.as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));
        let full = workspace.join("downloads").join(cleaned);

        // Ensure parent directory exists.
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| anyhow!("web_download: cannot create directory {}: {e}", parent.display()))?;
        }

        // Build cookie header: manual cookies param > auto from browser session
        let mut cookie_header = String::new();
        if let Some(cookies) = args["cookies"].as_str() {
            cookie_header = cookies.to_owned();
        } else if args["use_browser_cookies"].as_bool().unwrap_or(false) {
            // Extract cookies from active browser session via CDP
            let mut guard = self.browser.lock().await;
            if let Some(ref mut session) = *guard {
                match session.execute("cookies", &json!({})).await {
                    Ok(resp) => {
                        if let Some(cookies) = resp["cookies"].as_array() {
                            let url_parsed = reqwest::Url::parse(url).ok();
                            let domain = url_parsed.as_ref().and_then(|u| u.host_str());
                            let parts: Vec<String> = cookies.iter()
                                .filter(|c| {
                                    // Filter cookies matching the download URL domain
                                    if let (Some(d), Some(cd)) = (domain, c["domain"].as_str()) {
                                        let cd = cd.trim_start_matches('.');
                                        d == cd || d.ends_with(&format!(".{cd}"))
                                    } else {
                                        true
                                    }
                                })
                                .filter_map(|c| {
                                    let name = c["name"].as_str()?;
                                    let value = c["value"].as_str()?;
                                    Some(format!("{name}={value}"))
                                })
                                .collect();
                            cookie_header = parts.join("; ");
                            tracing::debug!(cookies_count = parts.len(), "web_download: extracted browser cookies");
                        }
                    }
                    Err(e) => {
                        tracing::warn!("web_download: failed to get browser cookies: {e}");
                    }
                }
            }
        }

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .timeout(Duration::from_secs(300))
            .build()?;

        // Resume support: if file exists, try Range request to continue download.
        let existing_size = tokio::fs::metadata(&full).await.map(|m| m.len()).unwrap_or(0);
        let mut req = client.get(url);
        if !cookie_header.is_empty() {
            req = req.header("Cookie", &cookie_header);
        }
        // Set Referer from URL origin — many CDNs (douyin, etc.) require it.
        if let Ok(parsed) = reqwest::Url::parse(url) {
            if let Some(origin) = parsed.host_str() {
                req = req.header("Referer", format!("{}://{}/", parsed.scheme(), origin));
            }
        }
        if existing_size > 0 {
            req = req.header("Range", format!("bytes={existing_size}-"));
        }

        let resp = req.send().await
            .map_err(|e| anyhow!("web_download: request failed: {e}"))?;

        if !resp.status().is_success() && resp.status().as_u16() != 206 {
            bail!("web_download: HTTP {} for {url}", resp.status());
        }

        // Warn if response is HTML (likely a redirect/login page, not the actual file).
        let content_type = resp.headers().get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        if content_type.contains("text/html") {
            bail!("web_download: server returned HTML instead of file. The URL may require different cookies or is a redirect page. Content-Type: {content_type}");
        }

        let resumed = resp.status().as_u16() == 206;

        // Stream to file (low memory). Append if resuming, create otherwise.
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut file = if resumed {
            tokio::fs::OpenOptions::new().append(true).open(&full).await
                .map_err(|e| anyhow!("web_download: cannot open for append {}: {e}", full.display()))?
        } else {
            tokio::fs::File::create(&full).await
                .map_err(|e| anyhow!("web_download: cannot create {}: {e}", full.display()))?
        };
        let mut downloaded: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow!("web_download: stream error: {e}"))?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;
        }
        file.flush().await?;

        let total = existing_size + downloaded;
        Ok(json!({
            "status": "ok",
            "path": full.to_string_lossy(),
            "size_bytes": total,
            "resumed": resumed,
        }))
    }

    async fn tool_web_browser(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("web_browser: `action` required"))?;

        // Get or init browser session. On each call we check if the existing
        // session has been idle for too long -- if so, drop it (ChromeProcess::Drop
        // kills the Chrome process) and reinitialize.
        {
            let mut guard = self.browser.lock().await;

            // Check if existing session is idle-expired; if so, drop it.
            if let Some(ref session) = *guard {
                if session.is_idle_expired() {
                    info!("Chrome idle timeout expired, closing session");
                    *guard = None;
                }
            }

            // Determine headed mode: per-request `headed` param overrides config.
            let wb_cfg = self.config.ext.tools.as_ref()
                .and_then(|t| t.web_browser.as_ref());
            let config_headed = wb_cfg.and_then(|b| b.headed).unwrap_or_else(has_display);
            let request_headed = args.get("headed").and_then(|v| v.as_bool());
            let headed = request_headed.unwrap_or(config_headed);
            let profile = wb_cfg.and_then(|b| b.profile.clone());

            // If headed mode changed, restart the session.
            if let Some(ref session) = *guard {
                if request_headed.is_some() && session.headed != headed {
                    info!(headed, "browser headed mode changed, restarting session");
                    *guard = None;
                }
            }

            // If no session, initialize one.
            if guard.is_none() {
                // Check Chrome availability
                let chrome_path = match wb_cfg
                    .and_then(|b| b.chrome_path.clone())
                    .or_else(|| detect_chrome())
                {
                    Some(p) => p,
                    None => {
                        let lang = crate::i18n::default_lang();
                        let msg = crate::i18n::t_fmt("tool_missing", lang, &[("tool", "chromium")]);
                        warn!("{}", msg);
                        if let Some(ref tx) = self.notification_tx {
                            let _ = tx.send(crate::channel::OutboundMessage {
                                target_id: ctx.peer_id.clone(),
                                is_group: false,
                                text: msg.clone(),
                                reply_to: None,
                                images: vec![],
                                files: vec![],
                                channel: Some(ctx.channel.clone()),
                            });
                        }
                        return Err(anyhow!(msg));
                    }
                };

                // Check memory before launching
                crate::browser::can_launch_chrome()?;

                let bs = crate::browser::BrowserSession::start(&chrome_path, headed, profile.as_deref()).await?;
                *guard = Some(bs);
            }
        }

        // Special action: capture_video — open page, inject interceptor, wait, collect video URLs.
        if action == "capture_video" {
            let url = args["url"].as_str().unwrap_or("");
            if url.is_empty() {
                bail!("capture_video: `url` required");
            }
            let wait_ms = args["wait_ms"].as_u64().unwrap_or(8000);

            let mut browser = self.browser.lock().await;
            let session = browser.as_mut().unwrap();

            // 1. Inject interceptor BEFORE navigating (catches all requests from start).
            let inject_js = r#"(function(){
                window.__vUrls=[];
                var xo=XMLHttpRequest.prototype.open;
                XMLHttpRequest.prototype.open=function(m,u){
                    if(u&&typeof u==='string'&&/video|mp4|m3u8|m4s|flv|playaddr|play_addr|pcdn|bilivideo/.test(u))
                        window.__vUrls.push(u);
                    return xo.apply(this,arguments);
                };
                var ff=window.fetch;
                window.fetch=function(u){
                    var s=typeof u==='string'?u:(u&&u.url||'');
                    if(/video|mp4|m3u8|m4s|flv|playaddr|play_addr|pcdn|bilivideo/.test(s))
                        window.__vUrls.push(s);
                    return ff.apply(this,arguments);
                };
                return 'interceptor_ready';
            })()"#;

            // 2. Navigate to the video page.
            session.execute("open", &json!({"url": url})).await?;
            tokio::time::sleep(Duration::from_millis(1000)).await;

            // 3. Inject interceptor (page scripts may have already loaded, so also check performance).
            let _ = session.execute("evaluate", &json!({"js": inject_js})).await;

            // 4. Wait for video to load/play.
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;

            // 5. Collect captured URLs + performance entries + video element src.
            let collect_js = r#"(function(){
                var urls = (window.__vUrls||[]).slice();
                try {
                    performance.getEntriesByType('resource').forEach(function(e){
                        if(e.name && /video|mp4|m3u8|m4s|flv|playaddr|play_addr|pcdn|bilivideo/.test(e.name)
                           && e.name.startsWith('http')
                           && !/poster|cover|thumbnail|preview/.test(e.name))
                            urls.push(e.name);
                    });
                } catch(e){}
                document.querySelectorAll('video,source').forEach(function(el){
                    var s = el.src || el.currentSrc || '';
                    if(s && s.startsWith('http')) urls.push(s);
                });
                return JSON.stringify([...new Set(urls)]);
            })()"#;

            let result = session.execute("evaluate", &json!({"js": collect_js})).await?;
            let urls_str = result["result"].as_str()
                .or_else(|| result.as_str())
                .unwrap_or("[]");

            let urls: Vec<String> = serde_json::from_str(urls_str).unwrap_or_default();

            // 6. If empty, try reload + re-collect.
            if urls.is_empty() {
                let _ = session.execute("evaluate", &json!({"js": "window.__vUrls=[];location.reload()"})).await;
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                let _ = session.execute("evaluate", &json!({"js": inject_js})).await;
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                let result2 = session.execute("evaluate", &json!({"js": collect_js})).await?;
                let urls_str2 = result2["result"].as_str()
                    .or_else(|| result2.as_str())
                    .unwrap_or("[]");
                let urls2: Vec<String> = serde_json::from_str(urls_str2).unwrap_or_default();
                return Ok(json!({
                    "video_urls": urls2,
                    "hint": if urls2.is_empty() { "No video URLs found. The page may require login or the video is DRM-protected." } else { "Pick the URL containing mp4/playaddr for download." }
                }));
            }

            return Ok(json!({
                "video_urls": urls,
                "hint": "Pick the URL containing mp4/playaddr for download. Use web_download with use_browser_cookies=true."
            }));
        }

        // Now lock again for execute -- guard is dropped, avoiding borrow issues.
        let mut browser = self.browser.lock().await;
        browser.as_mut().unwrap().execute(action, &args).await
    }

    async fn tool_computer_use(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("computer_use: `action` required"))?;

        let is_macos = cfg!(target_os = "macos");
        let is_windows = cfg!(target_os = "windows");

        // Helper: extract x, y from args
        let xy = || {
            (
                args["x"].as_f64().unwrap_or(0.0) as i64,
                args["y"].as_f64().unwrap_or(0.0) as i64,
            )
        };

        match action {
            // =================================================================
            // Screenshot — capture + auto-resize for HiDPI (saves tokens)
            // =================================================================
            "screenshot" => {
                let tmp_path = std::env::temp_dir().join("rsclaw_screen.png");
                let tmp_path_str = tmp_path.to_string_lossy().to_string();

                let output = if is_macos {
                    tokio::process::Command::new("screencapture")
                        .args(["-x", &tmp_path_str])
                        .output()
                        .await
                } else if is_windows {
                    let script = format!(
                        r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$screen = [System.Windows.Forms.Screen]::PrimaryScreen
$bounds = $screen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bitmap.Save('{}')
$graphics.Dispose()
$bitmap.Dispose()
"#,
                        tmp_path_str
                    );
                    powershell_hidden()
                        .args(["-Command", &script])
                        .output()
                        .await
                } else {
                    let res = tokio::process::Command::new("scrot")
                        .arg(&tmp_path_str)
                        .output()
                        .await;
                    if res.is_err() || !res.as_ref().unwrap().status.success() {
                        tokio::process::Command::new("import")
                            .args(["-window", "root", &tmp_path_str])
                            .output()
                            .await
                    } else {
                        res
                    }
                }
                .map_err(|e| anyhow!("computer_use screenshot: {e}"))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(anyhow!("computer_use screenshot failed: {stderr}"));
                }

                // Read raw PNG and get original dimensions from header.
                let raw_bytes = tokio::fs::read(&tmp_path)
                    .await
                    .map_err(|e| anyhow!("computer_use: failed to read screenshot: {e}"))?;
                let (orig_w, orig_h) = if raw_bytes.len() >= 24 {
                    let w = u32::from_be_bytes([raw_bytes[16], raw_bytes[17], raw_bytes[18], raw_bytes[19]]);
                    let h = u32::from_be_bytes([raw_bytes[20], raw_bytes[21], raw_bytes[22], raw_bytes[23]]);
                    (w, h)
                } else {
                    (0, 0)
                };

                // Resize to 1024px wide + convert to JPG q30 (~60KB).
                // Matches Anthropic's recommended XGA (1024x768) and saves
                // ~5-10x bandwidth vs raw PNG while maintaining OCR quality.
                const TARGET_WIDTH: u32 = 1024;
                const JPG_QUALITY: u32 = 30;

                let out_path = std::env::temp_dir().join("rsclaw_screen_out.jpg");
                let out_str = out_path.to_string_lossy().to_string();
                let need_resize = orig_w > TARGET_WIDTH;

                let converted = if is_macos {
                    // sips: resize + convert to JPEG in one pass
                    let mut sips_args = vec![];
                    if need_resize {
                        sips_args.extend_from_slice(&["--resampleWidth", "1024"]);
                    }
                    sips_args.extend_from_slice(&[
                        "-s", "format", "jpeg",
                        "-s", "formatOptions", "30",
                        &tmp_path_str,
                        "--out", &out_str,
                    ]);
                    tokio::process::Command::new("sips")
                        .args(&sips_args)
                        .output()
                        .await
                        .map(|o| o.status.success())
                        .unwrap_or(false)
                } else if is_windows {
                    let new_w = if need_resize { TARGET_WIDTH } else { orig_w };
                    let new_h = if need_resize {
                        (orig_h as f64 * TARGET_WIDTH as f64 / orig_w as f64) as u32
                    } else {
                        orig_h
                    };
                    let script = format!(
                        r#"
Add-Type -AssemblyName System.Drawing
$src = [System.Drawing.Image]::FromFile('{tmp_path_str}')
$dst = New-Object System.Drawing.Bitmap({new_w}, {new_h})
$g = [System.Drawing.Graphics]::FromImage($dst)
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.DrawImage($src, 0, 0, {new_w}, {new_h})
$codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object {{ $_.MimeType -eq 'image/jpeg' }}
$params = New-Object System.Drawing.Imaging.EncoderParameters(1)
$params.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter([System.Drawing.Imaging.Encoder]::Quality, [long]{JPG_QUALITY})
$dst.Save('{out_str}', $codec, $params)
$g.Dispose(); $dst.Dispose(); $src.Dispose()
"#
                    );
                    powershell_hidden()
                        .args(["-Command", &script])
                        .output()
                        .await
                        .map(|o| o.status.success())
                        .unwrap_or(false)
                } else {
                    // Linux: convert (ImageMagick)
                    let resize_arg = if need_resize { "1024x" } else { "100%" };
                    tokio::process::Command::new("convert")
                        .args([&tmp_path_str, "-resize", resize_arg, "-quality", "30", &out_str])
                        .output()
                        .await
                        .map(|o| o.status.success())
                        .unwrap_or(false)
                };

                // Use converted JPG if available, otherwise fall back to raw PNG.
                let (bytes, mime) = if converted {
                    let b = tokio::fs::read(&out_path).await.unwrap_or(raw_bytes);
                    let _ = tokio::fs::remove_file(&out_path).await;
                    (b, "image/jpeg")
                } else {
                    (raw_bytes, "image/png")
                };
                let _ = tokio::fs::remove_file(&tmp_path).await;

                // Get final dimensions. For JPEG, parse SOF0 marker; for PNG, use header.
                let (width, height) = if mime == "image/jpeg" {
                    jpeg_dimensions(&bytes).unwrap_or((0, 0))
                } else if bytes.len() >= 24 {
                    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
                    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
                    (w, h)
                } else {
                    (0, 0)
                };

                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

                // Return scale factor so LLM can map coordinates back.
                let scale = if width > 0 && orig_w > width { orig_w as f64 / width as f64 } else { 1.0 };

                Ok(json!({
                    "action": "screenshot",
                    "image": format!("data:{mime};base64,{b64}"),
                    "width": width,
                    "height": height,
                    "original_width": orig_w,
                    "original_height": orig_h,
                    "scale": scale
                }))
            }

            // =================================================================
            // Mouse move
            // =================================================================
            "mouse_move" => {
                let (x, y) = xy();
                if is_macos {
                    run_subprocess("cliclick", &[&format!("m:{x},{y}")]).await?;
                } else if is_windows {
                    win_set_cursor(x, y).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", &x.to_string(), &y.to_string()])
                        .await?;
                }
                Ok(json!({"action": "mouse_move", "ok": true}))
            }

            // =================================================================
            // Mouse click (left by default)
            // =================================================================
            "mouse_click" | "left_click" => {
                let (x, y) = xy();
                let button = args["button"].as_str().unwrap_or("left");
                if is_macos {
                    match button {
                        "right" => run_subprocess("cliclick", &[&format!("rc:{x},{y}")]).await?,
                        "middle" => {
                            // cliclick has no real middle-click; use CGEvent via swift
                            run_subprocess("swift", &["-e", &format!(
                                "import CoreGraphics; \
                                 let pt = CGPoint(x: {x}, y: {y}); \
                                 if let d = CGEvent(mouseEventSource: nil, mouseType: .otherMouseDown, mouseCursorPosition: pt, mouseButton: .center), \
                                    let u = CGEvent(mouseEventSource: nil, mouseType: .otherMouseUp, mouseCursorPosition: pt, mouseButton: .center) \
                                 {{ d.post(tap: .cghidEventTap); u.post(tap: .cghidEventTap) }}"
                            )]).await?;
                        }
                        _ => run_subprocess("cliclick", &[&format!("c:{x},{y}")]).await?,
                    }
                } else if is_windows {
                    win_mouse_click(x, y, button, 1).await?;
                } else {
                    let btn = match button { "right" => "3", "middle" => "2", _ => "1" };
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", btn]).await?;
                }
                Ok(json!({"action": "mouse_click", "button": button, "ok": true}))
            }

            // =================================================================
            // Double click
            // =================================================================
            "double_click" => {
                let (x, y) = xy();
                if is_macos {
                    run_subprocess("cliclick", &[&format!("dc:{x},{y}")]).await?;
                } else if is_windows {
                    win_mouse_click(x, y, "left", 2).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", "--repeat", "2", "--delay", "50", "1"]).await?;
                }
                Ok(json!({"action": "double_click", "ok": true}))
            }

            // =================================================================
            // Triple click (select whole line)
            // =================================================================
            "triple_click" => {
                let (x, y) = xy();
                if is_macos {
                    // cliclick has no tc: command; use three rapid clicks
                    let pos = format!("c:{x},{y}");
                    run_subprocess("cliclick", &[&pos, &pos, &pos]).await?;
                } else if is_windows {
                    win_mouse_click(x, y, "left", 3).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", "--repeat", "3", "--delay", "50", "1"]).await?;
                }
                Ok(json!({"action": "triple_click", "ok": true}))
            }

            // =================================================================
            // Right click
            // =================================================================
            "right_click" => {
                let (x, y) = xy();
                if is_macos {
                    run_subprocess("cliclick", &[&format!("rc:{x},{y}")]).await?;
                } else if is_windows {
                    win_mouse_click(x, y, "right", 1).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", "3"]).await?;
                }
                Ok(json!({"action": "right_click", "ok": true}))
            }

            // =================================================================
            // Middle click
            // =================================================================
            "middle_click" => {
                let (x, y) = xy();
                if is_macos {
                    // cliclick has no real middle-click; use CGEvent via swift
                    run_subprocess("swift", &["-e", &format!(
                        "import CoreGraphics; \
                         let pt = CGPoint(x: {x}, y: {y}); \
                         if let d = CGEvent(mouseEventSource: nil, mouseType: .otherMouseDown, mouseCursorPosition: pt, mouseButton: .center), \
                            let u = CGEvent(mouseEventSource: nil, mouseType: .otherMouseUp, mouseCursorPosition: pt, mouseButton: .center) \
                         {{ d.post(tap: .cghidEventTap); u.post(tap: .cghidEventTap) }}"
                    )]).await?;
                } else if is_windows {
                    win_mouse_click(x, y, "middle", 1).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", "2"]).await?;
                }
                Ok(json!({"action": "middle_click", "ok": true}))
            }

            // =================================================================
            // Drag (from x1,y1 to x2,y2)
            // =================================================================
            "drag" => {
                let x1 = args["x"].as_f64().unwrap_or(0.0) as i64;
                let y1 = args["y"].as_f64().unwrap_or(0.0) as i64;
                let x2 = args["to_x"].as_f64()
                    .ok_or_else(|| anyhow!("computer_use drag: `to_x` required"))? as i64;
                let y2 = args["to_y"].as_f64()
                    .ok_or_else(|| anyhow!("computer_use drag: `to_y` required"))? as i64;
                if is_macos {
                    run_subprocess("cliclick", &[&format!("dd:{x1},{y1}"), &format!("du:{x2},{y2}")]).await?;
                } else if is_windows {
                    run_powershell_input(&format!(
                        r#"Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinDrag {{
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern void mouse_event(uint f, uint dx, uint dy, uint d, int e);
    public static void Drag(int x1, int y1, int x2, int y2) {{
        SetCursorPos(x1, y1);
        System.Threading.Thread.Sleep(50);
        mouse_event(0x0002, 0, 0, 0, 0); // LEFTDOWN
        System.Threading.Thread.Sleep(50);
        SetCursorPos(x2, y2);
        System.Threading.Thread.Sleep(50);
        mouse_event(0x0004, 0, 0, 0, 0); // LEFTUP
    }}
}}
"@
[WinDrag]::Drag({x1}, {y1}, {x2}, {y2})"#
                    )).await?;
                } else {
                    run_subprocess("xdotool", &[
                        "mousemove", &x1.to_string(), &y1.to_string(),
                        "mousedown", "1",
                        "mousemove", "--sync", &x2.to_string(), &y2.to_string(),
                        "mouseup", "1",
                    ]).await?;
                }
                Ok(json!({"action": "drag", "from": [x1, y1], "to": [x2, y2], "ok": true}))
            }

            // =================================================================
            // Scroll (direction: up/down/left/right, amount: clicks)
            // =================================================================
            "scroll" => {
                let (x, y) = xy();
                let direction = args["direction"].as_str().unwrap_or("down");
                let amount = args["amount"].as_i64().unwrap_or(3);
                if is_macos {
                    if x != 0 || y != 0 {
                        run_subprocess("cliclick", &[&format!("m:{x},{y}")]).await?;
                    }
                    // Use CGEvent scroll via swift (macOS built-in, no deps)
                    let (scroll_y, scroll_x) = match direction {
                        "up" => (amount, 0i64),
                        "down" => (-amount, 0),
                        "left" => (0, -amount),
                        "right" => (0, amount),
                        _ => (-amount, 0),
                    };
                    run_subprocess("swift", &["-e", &format!(
                        "import CoreGraphics; \
                         if let e = CGEvent(scrollWheelEvent2Source: nil, units: .line, \
                         wheelCount: 2, wheel1: Int32({scroll_y}), wheel2: Int32({scroll_x}), wheel3: 0) \
                         {{ e.post(tap: .cghidEventTap) }}"
                    )]).await?;
                } else if is_windows {
                    if x != 0 || y != 0 {
                        win_set_cursor(x, y).await?;
                    }
                    let (wheel_flag, delta) = match direction {
                        "up" => ("0x0800", 120 * amount),    // MOUSEEVENTF_WHEEL
                        "down" => ("0x0800", -120 * amount),
                        "left" => ("0x01000", 120 * amount), // MOUSEEVENTF_HWHEEL
                        "right" => ("0x01000", -120 * amount),
                        _ => ("0x0800", -120 * amount),
                    };
                    run_powershell_input(&format!(
                        r#"Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinScroll {{
    [DllImport("user32.dll")] static extern void mouse_event(uint f, uint dx, uint dy, int d, int e);
    public static void Scroll(uint flag, int delta) {{
        mouse_event(flag, 0, 0, delta, 0);
    }}
}}
"@
[WinScroll]::Scroll({wheel_flag}, {delta})"#
                    )).await?;
                } else {
                    if x != 0 || y != 0 {
                        run_subprocess("xdotool", &["mousemove", &x.to_string(), &y.to_string()]).await?;
                    }
                    let btn = match direction {
                        "up" => "4", "down" => "5", "left" => "6", "right" => "7", _ => "5",
                    };
                    run_subprocess("xdotool", &["click", "--repeat", &amount.to_string(), "--delay", "30", btn]).await?;
                }
                Ok(json!({"action": "scroll", "direction": direction, "amount": amount, "ok": true}))
            }

            // =================================================================
            // Type text
            // =================================================================
            "type" => {
                let text = args["text"]
                    .as_str()
                    .ok_or_else(|| anyhow!("computer_use type: `text` required"))?;
                if is_macos {
                    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
                    run_subprocess(
                        "osascript",
                        &["-e", &format!("tell application \"System Events\" to keystroke \"{escaped}\"")],
                    ).await?;
                } else if is_windows {
                    // Escape SendKeys special chars: + ^ % ~ { } ( )
                    // Must handle { } carefully to avoid double-escaping
                    let mut escaped = String::with_capacity(text.len() * 2);
                    for ch in text.chars() {
                        match ch {
                            '{' => escaped.push_str("{{}"),
                            '}' => escaped.push_str("{}}"),
                            '+' => escaped.push_str("{+}"),
                            '^' => escaped.push_str("{^}"),
                            '%' => escaped.push_str("{%}"),
                            '~' => escaped.push_str("{~}"),
                            '(' => escaped.push_str("{(}"),
                            ')' => escaped.push_str("{)}"),
                            '\'' => escaped.push_str("''"),
                            other => escaped.push(other),
                        }
                    }
                    run_powershell_input(&format!(
                        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{escaped}')"
                    )).await?;
                } else {
                    run_subprocess("xdotool", &["type", "--clearmodifiers", text]).await?;
                }
                Ok(json!({"action": "type", "ok": true}))
            }

            // =================================================================
            // Key press (single key or combo like "ctrl+c")
            // =================================================================
            "key" => {
                let key = args["key"]
                    .as_str()
                    .ok_or_else(|| anyhow!("computer_use key: `key` required"))?;
                if is_macos {
                    // cliclick kp: only supports special key names (return, esc, f1, etc.)
                    // For regular characters, use t: (type). For combos, use kd:/ku: + t: or kp:.
                    if key.contains('+') {
                        let parts: Vec<&str> = key.split('+').collect();
                        let mut cliclick_args: Vec<String> = Vec::new();
                        for &modifier in &parts[..parts.len() - 1] {
                            let m = map_modifier(modifier);
                            cliclick_args.push(format!("kd:{m}"));
                        }
                        let base = parts[parts.len() - 1];
                        // Use kp: for special keys, t: for regular characters
                        if is_cliclick_special_key(base) {
                            cliclick_args.push(format!("kp:{base}"));
                        } else {
                            cliclick_args.push(format!("t:{base}"));
                        }
                        for &modifier in parts[..parts.len() - 1].iter().rev() {
                            let m = map_modifier(modifier);
                            cliclick_args.push(format!("ku:{m}"));
                        }
                        let refs: Vec<&str> = cliclick_args.iter().map(|s| s.as_str()).collect();
                        run_subprocess("cliclick", &refs).await?;
                    } else if is_cliclick_special_key(key) {
                        run_subprocess("cliclick", &[&format!("kp:{key}")]).await?;
                    } else {
                        // Single regular character — use osascript keystroke
                        let escaped = key.replace('\\', "\\\\").replace('"', "\\\"");
                        run_subprocess("osascript", &[
                            "-e", &format!("tell application \"System Events\" to keystroke \"{escaped}\""),
                        ]).await?;
                    }
                } else if is_windows {
                    let send_key = win_map_key(key);
                    let escaped = send_key.replace('\'', "''");
                    run_powershell_input(&format!(
                        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{escaped}')"
                    )).await?;
                } else {
                    run_subprocess("xdotool", &["key", key]).await?;
                }
                Ok(json!({"action": "key", "ok": true}))
            }

            // =================================================================
            // Hold key + click (e.g. Shift+Click for multi-select)
            // =================================================================
            "hold_key" => {
                let key = args["key"]
                    .as_str()
                    .ok_or_else(|| anyhow!("computer_use hold_key: `key` required"))?;
                let (x, y) = xy();
                let sub_action = args["then"].as_str().unwrap_or("click");
                if is_macos {
                    let m = map_modifier(key);
                    let click_cmd = match sub_action {
                        "double_click" => format!("dc:{x},{y}"),
                        "right_click" => format!("rc:{x},{y}"),
                        _ => format!("c:{x},{y}"),
                    };
                    run_subprocess("cliclick", &[&format!("kd:{m}"), &click_cmd, &format!("ku:{m}")]).await?;
                } else if is_windows {
                    let key_lower = key.to_lowercase();
                    let vk = match key_lower.as_str() {
                        "ctrl" | "control" => "0x11",
                        "alt" => "0x12",
                        "shift" => "0x10",
                        "win" | "super" | "cmd" | "command" => "0x5B",
                        _ => "0x10", // default to shift
                    };
                    let clicks = match sub_action { "double_click" => 2, "triple_click" => 3, _ => 1 };
                    let btn_down = match sub_action { "right_click" => "0x0008", _ => "0x0002" };
                    let btn_up = match sub_action { "right_click" => "0x0010", _ => "0x0004" };
                    run_powershell_input(&format!(
                        r#"Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinHoldKey {{
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern void mouse_event(uint f, uint dx, uint dy, uint d, int e);
    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, int extra);
    public static void HoldAndClick(int x, int y, byte vk, uint down, uint up, int clicks) {{
        keybd_event(vk, 0, 0, 0); // key down
        SetCursorPos(x, y);
        for (int i = 0; i < clicks; i++) {{
            mouse_event(down, 0, 0, 0, 0);
            mouse_event(up, 0, 0, 0, 0);
            if (i < clicks - 1) System.Threading.Thread.Sleep(50);
        }}
        keybd_event(vk, 0, 2, 0); // key up (KEYEVENTF_KEYUP=2)
    }}
}}
"@
[WinHoldKey]::HoldAndClick({x}, {y}, {vk}, {btn_down}, {btn_up}, {clicks})"#
                    )).await?;
                } else {
                    let xdo_key = map_modifier_xdotool(key);
                    run_subprocess("xdotool", &["mousemove", &x.to_string(), &y.to_string()]).await?;
                    run_subprocess("xdotool", &["keydown", &xdo_key]).await?;
                    let repeat = match sub_action { "double_click" => "2", "triple_click" => "3", _ => "1" };
                    let btn = match sub_action { "right_click" => "3", _ => "1" };
                    run_subprocess("xdotool", &["click", "--repeat", repeat, "--delay", "50", btn]).await?;
                    run_subprocess("xdotool", &["keyup", &xdo_key]).await?;
                }
                Ok(json!({"action": "hold_key", "key": key, "then": sub_action, "ok": true}))
            }

            // =================================================================
            // Cursor position — get current mouse location
            // =================================================================
            "cursor_position" => {
                let pos = if is_macos {
                    let output = tokio::process::Command::new("cliclick")
                        .arg("p:.")
                        .output()
                        .await
                        .map_err(|e| anyhow!("cliclick: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                } else if is_windows {
                    let output = powershell_hidden()
                        .args(["-Command",
                            "Add-Type -AssemblyName System.Windows.Forms; $p = [System.Windows.Forms.Cursor]::Position; \"$($p.X),$($p.Y)\""])
                        .output()
                        .await
                        .map_err(|e| anyhow!("powershell: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                } else {
                    let output = tokio::process::Command::new("xdotool")
                        .args(["getmouselocation", "--shell"])
                        .output()
                        .await
                        .map_err(|e| anyhow!("xdotool: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                };
                // Parse "x,y" format
                let parts: Vec<&str> = pos.split(',').collect();
                let (cx, cy) = if parts.len() >= 2 {
                    (
                        parts[0].trim().parse::<i64>().unwrap_or(0),
                        parts[1].trim().parse::<i64>().unwrap_or(0),
                    )
                } else {
                    // xdotool --shell format: X=123\nY=456
                    let mut cx = 0i64;
                    let mut cy = 0i64;
                    for line in pos.lines() {
                        if let Some(v) = line.strip_prefix("X=") {
                            cx = v.parse().unwrap_or(0);
                        } else if let Some(v) = line.strip_prefix("Y=") {
                            cy = v.parse().unwrap_or(0);
                        }
                    }
                    (cx, cy)
                };
                Ok(json!({"action": "cursor_position", "x": cx, "y": cy}))
            }

            // =================================================================
            // Get active window title — context awareness
            // =================================================================
            "get_active_window" => {
                let title = if is_macos {
                    let output = tokio::process::Command::new("osascript")
                        .args(["-e", "tell application \"System Events\" to get name of first process whose frontmost is true"])
                        .output()
                        .await
                        .map_err(|e| anyhow!("osascript: {e}"))?;
                    let app = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    // Also get window title
                    let output2 = tokio::process::Command::new("osascript")
                        .args(["-e", "tell application \"System Events\" to get name of front window of (first process whose frontmost is true)"])
                        .output()
                        .await;
                    let win = output2.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
                    if win.is_empty() { app } else { format!("{app} — {win}") }
                } else if is_windows {
                    let output = powershell_hidden()
                        .args(["-Command",
                            "Add-Type @\"\nusing System;\nusing System.Runtime.InteropServices;\npublic class WinTitle {\n  [DllImport(\"user32.dll\")] static extern IntPtr GetForegroundWindow();\n  [DllImport(\"user32.dll\")] static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);\n  public static string Get() {\n    var sb = new System.Text.StringBuilder(256);\n    GetWindowText(GetForegroundWindow(), sb, 256);\n    return sb.ToString();\n  }\n}\n\"@\n[WinTitle]::Get()"])
                        .output()
                        .await
                        .map_err(|e| anyhow!("powershell: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                } else {
                    let output = tokio::process::Command::new("xdotool")
                        .args(["getactivewindow", "getwindowname"])
                        .output()
                        .await
                        .map_err(|e| anyhow!("xdotool: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                };
                Ok(json!({"action": "get_active_window", "title": title}))
            }

            // =================================================================
            // Wait — pause between actions (ms)
            // =================================================================
            "wait" => {
                let ms = args["ms"].as_u64().unwrap_or(500).min(10000); // cap at 10s
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok(json!({"action": "wait", "ms": ms, "ok": true}))
            }

            other => Err(anyhow!(
                "computer_use: unsupported action `{other}` \
                 (supported: screenshot, mouse_move, mouse_click, double_click, triple_click, \
                 right_click, middle_click, drag, scroll, type, key, hold_key, cursor_position, \
                 get_active_window, wait)"
            )),
        }
    }

    // -----------------------------------------------------------------------
    // New openclaw-compatible tools
    // -----------------------------------------------------------------------

    async fn tool_image(&self, args: Value) -> Result<Value> {
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| anyhow!("image: `prompt` required"))?;

        // Check user-configured image model: agents.defaults.model.image
        let user_image_model = self
            .handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.image.as_deref())
            .or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.image.as_deref())
            })
            .map(|s| s.to_owned());

        // Resolve provider — from image model config or current chat model
        let resolve_model = user_image_model.clone().unwrap_or_else(|| self.resolve_model_name());
        let (prov_name, user_model_id) = {
            crate::provider::registry::ProviderRegistry::parse_model(&resolve_model)
        };
        let (base_url, _auth_style) = crate::provider::defaults::resolve_base_url(prov_name);

        let default_size = match prov_name {
            _ => "2048x2048",
        };
        let size = args["size"].as_str().unwrap_or(default_size);

        // Also check provider config for api_key and base_url overrides
        let cfg_key = self
            .config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get(prov_name))
            .and_then(|p| p.api_key.as_ref())
            .and_then(|k| k.as_plain().map(str::to_owned));
        let cfg_url = self
            .config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get(prov_name))
            .and_then(|p| p.base_url.clone());

        // Providers with image generation support
        let image_providers = ["doubao", "bytedance", "openai", "qwen", "minimax", "gemini"];
        let (img_url, img_key, img_prov) = if image_providers.contains(&prov_name) {
            let url = cfg_url.unwrap_or(base_url);
            let key = cfg_key
                .or_else(|| std::env::var(format!("{}_API_KEY", prov_name.to_uppercase())).ok())
                .or_else(|| std::env::var("OPENAI_API_KEY").ok());
            (url, key, prov_name)
        } else {
            // Current provider doesn't support images — try doubao, qwen, openai
            let fallback = [("doubao", "ARK_API_KEY"), ("qwen", "DASHSCOPE_API_KEY"), ("minimax", "MINIMAX_API_KEY"), ("gemini", "GEMINI_API_KEY"), ("openai", "OPENAI_API_KEY")];
            let mut found = None;
            for (fb_prov, fb_env) in fallback {
                let fb_cfg = self
                    .config
                    .model
                    .models
                    .as_ref()
                    .and_then(|m| m.providers.get(fb_prov));
                let fb_key = fb_cfg
                    .and_then(|p| p.api_key.as_ref())
                    .and_then(|k| k.as_plain().map(str::to_owned))
                    .or_else(|| std::env::var(fb_env).ok());
                if let Some(key) = fb_key {
                    let fb_url = fb_cfg
                        .and_then(|p| p.base_url.clone())
                        .unwrap_or_else(|| crate::provider::defaults::resolve_base_url(fb_prov).0);
                    found = Some((fb_url, Some(key), fb_prov));
                    break;
                }
            }
            found.unwrap_or_else(|| (cfg_url.unwrap_or(base_url), None, prov_name))
        };
        let Some(api_key) = img_key else {
            return Ok(json!({
                "error": "AI image generation requires doubao, qwen, minimax, gemini, or openai provider with API key. No image-capable provider configured."
            }));
        };

        let image_model = args["model"].as_str()
            .or_else(|| if !user_model_id.is_empty() { Some(user_model_id) } else { None })
            .unwrap_or_else(|| match img_prov {
                "doubao" | "bytedance" => "doubao-seedream-5-0-260128",
                "openai" => "dall-e-3",
                "qwen" => "qwen-image-2.0-pro",
                "minimax" => "image-01",
                "gemini" => "gemini-3-pro-image-preview",
                _ => "dall-e-3",
            });

        // Resolve User-Agent: provider config → gateway config → default
        let img_ua = self.config.model.models.as_ref()
            .and_then(|m| m.providers.get(img_prov))
            .and_then(|p| p.user_agent.as_deref())
            .or_else(|| self.config.gateway.user_agent.as_deref())
            .unwrap_or(crate::provider::DEFAULT_USER_AGENT);
        let client = reqwest::Client::builder()
            .user_agent(img_ua)
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default();

        tracing::info!(provider = img_prov, model = image_model, size = size, ua = img_ua, "tool_image: generating");

        // Provider-specific API formats
        let is_qwen = img_prov == "qwen";
        let is_minimax = img_prov == "minimax";
        let is_gemini = img_prov == "gemini";
        let (resp_status, resp_body) = if is_qwen {
            let qwen_size = size.replace('x', "*");
            let resp = client
                .post("https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation")
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({
                    "model": image_model,
                    "input": { "messages": [{ "role": "user", "content": [{ "text": prompt }] }] },
                    "parameters": { "size": qwen_size, "n": 1, "watermark": false }
                }))
                .send().await
                .map_err(|e| anyhow!("image: request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| anyhow!("image: parse error: {e}"))?;
            (st, body)
        } else if is_minimax {
            // Minimax: /v1/image_generation, aspect_ratio instead of size
            // Supported: "1:1", "16:9", "9:16", "4:3", "3:4", "2:3", "3:2"
            let aspect = if size.contains('x') {
                let parts: Vec<&str> = size.split('x').collect();
                if parts.len() == 2 {
                    let w = parts[0].parse::<f32>().unwrap_or(1024.0);
                    let h = parts[1].parse::<f32>().unwrap_or(1024.0);
                    let ratio = w / h.max(1.0);
                    let candidates = [
                        (1.0_f32, "1:1"),
                        (16.0 / 9.0, "16:9"),
                        (9.0 / 16.0, "9:16"),
                        (4.0 / 3.0, "4:3"),
                        (3.0 / 4.0, "3:4"),
                        (3.0 / 2.0, "3:2"),
                        (2.0 / 3.0, "2:3"),
                    ];
                    candidates
                        .iter()
                        .min_by(|a, b| {
                            (a.0 - ratio)
                                .abs()
                                .partial_cmp(&(b.0 - ratio).abs())
                                .unwrap()
                        })
                        .map(|c| c.1)
                        .unwrap_or("1:1")
                        .to_owned()
                } else {
                    "1:1".to_owned()
                }
            } else {
                "1:1".to_owned()
            };
            let url = format!("{}/image_generation", img_url.trim_end_matches('/'));
            let resp = client.post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({ "model": image_model, "prompt": prompt, "aspect_ratio": aspect, "response_format": "url" }))
                .send().await
                .map_err(|e| anyhow!("image: request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| anyhow!("image: parse error: {e}"))?;
            (st, body)
        } else if is_gemini {
            // Gemini: generateContent with responseModalities: ["IMAGE"]
            // Map size to aspect ratio for Gemini
            let aspect = if size.contains('x') {
                let parts: Vec<&str> = size.split('x').collect();
                if parts.len() == 2 {
                    let w = parts[0].parse::<u32>().unwrap_or(2048);
                    let h = parts[1].parse::<u32>().unwrap_or(2048);
                    if w == h { "1:1" } else if w > h { "16:9" } else { "9:16" }
                } else { "1:1" }
            } else { "1:1" };
            let gemini_base = img_url.trim_end_matches('/');
            let url = format!("{gemini_base}/models/{image_model}:generateContent?key={api_key}");
            let resp = client.post(&url)
                .json(&json!({
                    "contents": [{ "parts": [{ "text": prompt }] }],
                    "generationConfig": {
                        "responseModalities": ["TEXT", "IMAGE"],
                        "imageConfig": { "aspectRatio": aspect }
                    }
                }))
                .send().await
                .map_err(|e| anyhow!("image: gemini request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp.json().await.map_err(|e| anyhow!("image: gemini parse error: {e}"))?;
            (st, body)
        } else {
            let url = format!("{}/images/generations", img_url.trim_end_matches('/'));
            let resp = client.post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({ "model": image_model, "prompt": prompt, "size": size, "n": 1, "response_format": "url" }))
                .send().await
                .map_err(|e| anyhow!("image: request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| anyhow!("image: parse error: {e}"))?;
            (st, body)
        };

        if !resp_status.is_success() {
            let err_msg = resp_body["error"]["message"]
                .as_str()
                .or_else(|| resp_body["message"].as_str())
                .unwrap_or("unknown error");
            return Err(anyhow!("image: API error: {err_msg}"));
        }

        // Extract image URL/base64 — different response formats per provider
        // Gemini returns inline base64 directly, others return URLs
        if is_gemini {
            // Gemini: candidates[0].content.parts[] — find the inlineData part
            #[allow(unused_imports)]
            use base64::Engine;
            let parts = resp_body.pointer("/candidates/0/content/parts")
                .and_then(|v| v.as_array());
            if let Some(parts) = parts {
                for part in parts {
                    if let Some(inline) = part.get("inlineData") {
                        let mime = inline.get("mimeType").and_then(|v| v.as_str()).unwrap_or("image/png");
                        if let Some(b64_data) = inline.get("data").and_then(|v| v.as_str()) {
                            let data_uri = format!("data:{mime};base64,{b64_data}");
                            return Ok(json!({
                                "url": data_uri,
                                "revised_prompt": prompt
                            }));
                        }
                    }
                }
            }
            return Err(anyhow!("image: no image data in Gemini response"));
        }

        let img_url_str = if is_qwen {
            resp_body
                .pointer("/output/choices/0/message/content/0/image")
                .and_then(|v| v.as_str())
        } else if is_minimax {
            // minimax: data.image_base64[0] (base64) or data.image_urls[0] (url)
            resp_body.pointer("/data/image_urls/0").and_then(|v| v.as_str())
                .or_else(|| resp_body.pointer("/data/image_base64/0").and_then(|v| v.as_str()))
        } else {
            resp_body.pointer("/data/0/url").and_then(|v| v.as_str())
        };

        let Some(img_url_str) = img_url_str else {
            return Err(anyhow!("image: no image URL in response"));
        };

        // Download image and convert to data URI
        use base64::Engine;
        let image_result = match reqwest::Client::new()
            .get(img_url_str)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(bytes) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    format!("data:image/png;base64,{b64}")
                }
                Err(e) => return Err(anyhow!("image: download failed: {e}")),
            },
            Ok(r) => return Err(anyhow!("image: download returned {}", r.status())),
            Err(e) => return Err(anyhow!("image: download error: {e}")),
        };

        let revised = resp_body
            .pointer("/data/0/revised_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        Ok(json!({
            "url": image_result,
            "revised_prompt": revised,
            "size": size,
            "model": image_model
        }))
    }

    async fn tool_pdf(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("pdf: `path` required"))?;

        // If URL, download to temp file first.
        let local_path = if path.starts_with("http://") || path.starts_with("https://") {
            let tmp = std::env::temp_dir().join("rsclaw_pdf_download.pdf");
            let client = reqwest::Client::new();
            let bytes = client
                .get(path)
                .send()
                .await
                .map_err(|e| anyhow!("pdf: download failed: {e}"))?
                .bytes()
                .await
                .map_err(|e| anyhow!("pdf: download read failed: {e}"))?;
            tokio::fs::write(&tmp, &bytes)
                .await
                .map_err(|e| anyhow!("pdf: write temp file failed: {e}"))?;
            tmp
        } else {
            std::path::PathBuf::from(path)
        };

        // Pure Rust PDF extraction, with pdftotext CLI fallback.
        let pdf_bytes = tokio::fs::read(&local_path)
            .await
            .map_err(|e| anyhow!("pdf: read failed: {e}"))?;
        let text = match crate::agent::doc::safe_extract_pdf_from_mem(&pdf_bytes) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("pdf-extract failed ({e}), trying pdftotext CLI");
                let output = tokio::process::Command::new("pdftotext")
                    .args([local_path.to_str().unwrap_or(""), "-"])
                    .output()
                    .await
                    .map_err(|e2| anyhow!("pdf: extraction failed: {e}, pdftotext: {e2}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(anyhow!("pdf: extraction failed: {e}, pdftotext: {stderr}"));
                }
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
        };
        // Truncate to 100k chars to avoid blowing up context.
        let truncated = if text.len() > 100_000 {
            let mut end = 100_000usize;
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
            format!("{}...\n[truncated at 100000 chars]", &text[..end])
        } else {
            text
        };

        Ok(json!({
            "path": path,
            "text": truncated,
            "chars": truncated.len()
        }))
    }

    /// Generate TTS audio from text. Prefers sherpa-onnx, falls back to system TTS.
    /// Returns the path to the generated audio file.
    async fn generate_tts_audio(&self, text: &str) -> Result<String> {
        // Truncate long text for TTS (avoid very long audio).
        let tts_text = if text.chars().count() > 500 {
            let idx = text.char_indices().nth(500).map(|(i, _)| i).unwrap_or(text.len());
            &text[..idx]
        } else {
            text
        };

        let out_path = std::env::temp_dir().join(format!(
            "rsclaw_tts_{}.wav",
            chrono::Utc::now().timestamp_millis()
        ));
        let out_str = out_path.to_string_lossy().to_string();

        // Try sherpa-onnx first (installed via `rsclaw tools install sherpa-onnx`).
        let sherpa_bin = crate::config::loader::base_dir()
            .join("tools")
            .join("sherpa-onnx")
            .join("bin")
            .join(if cfg!(target_os = "windows") { "sherpa-onnx-offline-tts.exe" } else { "sherpa-onnx-offline-tts" });

        if sherpa_bin.exists() {
            let model_dir = crate::config::loader::base_dir()
                .join("tools")
                .join("sherpa-onnx")
                .join("models")
                .join("tts");
            // Look for any VITS model config.
            let model_config = model_dir.join("model.onnx");
            if model_config.exists() {
                let mut cmd = tokio::process::Command::new(&sherpa_bin);
                cmd.args([
                    "--vits-model", model_config.to_str().unwrap_or(""),
                    "--vits-tokens", model_dir.join("tokens.txt").to_str().unwrap_or(""),
                    "--output-filename", &out_str,
                    "--vits-length-scale", "1.0",
                    tts_text,
                ]);
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                let output = cmd.output().await;
                if let Ok(o) = output {
                    if o.status.success() && out_path.exists() {
                        return Ok(out_str);
                    }
                }
                // Fall through to system TTS if sherpa-onnx failed.
            }
        }

        // Fallback: system TTS (same as tool_tts).
        #[cfg(target_os = "macos")]
        {
            let output = tokio::process::Command::new("say")
                .args(["-o", &out_str, tts_text])
                .output()
                .await
                .map_err(|e| anyhow!("auto-tts: say failed: {e}"))?;
            if !output.status.success() {
                return Err(anyhow!("auto-tts: say exit code {}", output.status));
            }
        }
        #[cfg(target_os = "windows")]
        {
            let safe_text = tts_text.replace('\'', "''");
            let script = format!(
                "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; $s.SetOutputToWaveFile('{}'); $s.Speak('{}')",
                out_str.replace('\'', "''"), safe_text
            );
            let output = powershell_hidden()
                .args(["-Command", &script])
                .output()
                .await
                .map_err(|e| anyhow!("auto-tts: SAPI failed: {e}"))?;
            if !output.status.success() {
                return Err(anyhow!("auto-tts: SAPI exit code {}", output.status));
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let result = tokio::process::Command::new("espeak")
                .args(["-w", &out_str, tts_text])
                .output()
                .await;
            match result {
                Ok(o) if o.status.success() => {}
                _ => {
                    tokio::process::Command::new("pico2wave")
                        .args(["-w", &out_str, "--", tts_text])
                        .output()
                        .await
                        .map_err(|e| anyhow!("auto-tts: no TTS engine available: {e}"))?;
                }
            }
        }

        if out_path.exists() {
            Ok(out_str)
        } else {
            Err(anyhow!("auto-tts: output file not created"))
        }
    }

    async fn tool_tts(&self, args: Value) -> Result<Value> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("tts: `text` required"))?;
        let voice = args["voice"].as_str().unwrap_or("default");

        let out_path = std::env::temp_dir().join(format!(
            "rsclaw_tts_{}{}",
            chrono::Utc::now().timestamp_millis(),
            if cfg!(target_os = "windows") {
                ".wav"
            } else {
                ".aiff"
            }
        ));
        let out_path_str = out_path.to_string_lossy().to_string();

        let is_macos = cfg!(target_os = "macos");
        let is_windows = cfg!(target_os = "windows");

        if is_macos {
            let mut cmd = tokio::process::Command::new("say");
            if voice != "default" {
                cmd.args(["-v", voice]);
            }
            cmd.args(["-o", &out_path_str, text]);
            let output = cmd
                .output()
                .await
                .map_err(|e| anyhow!("tts: `say` command failed: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow!("tts: say failed: {stderr}"));
            }
        } else if is_windows {
            let script = format!(
                r#"
Add-Type -AssemblyName System.Speech
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
$synth.SetOutputToWaveFile('{}')
$synth.Speak('{}')
"#,
                out_path_str, text
            );
            let output = powershell_hidden()
                .args(["-Command", &script])
                .output()
                .await
                .map_err(|e| anyhow!("tts: PowerShell SAPI failed: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow!("tts: SAPI failed: {stderr}"));
            }
        } else {
            let espeak_result = tokio::process::Command::new("espeak")
                .args(["-w", &out_path_str, text])
                .output()
                .await;
            match espeak_result {
                Ok(o) if o.status.success() => {}
                _ => {
                    let output = tokio::process::Command::new("pico2wave")
                        .args(["-w", &out_path_str, "--", text])
                        .output()
                        .await
                        .map_err(|e| anyhow!("tts: neither espeak nor pico2wave available: {e}"))?;
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return Err(anyhow!("tts: pico2wave failed: {stderr}"));
                    }
                }
            }
        }

        Ok(json!({
            "audio_file": out_path_str,
            "voice": voice,
            "chars": text.len()
        }))
    }

    async fn tool_message(&self, args: Value) -> Result<Value> {
        let target = args["target"]
            .as_str()
            .ok_or_else(|| anyhow!("message: `target` required"))?;
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("message: `text` required"))?;
        let channel = args["channel"].as_str().unwrap_or("default");

        // Try to POST to the gateway's own message-send endpoint.
        let port = self.config.gateway.port;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/v1/message/send"))
            .json(&json!({
                "channel": channel,
                "target": target,
                "text": text
            }))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let body: Value = r.json().await.unwrap_or(json!({"ok": true}));
                Ok(json!({
                    "sent": true,
                    "channel": channel,
                    "target": target,
                    "response": body
                }))
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                Err(anyhow!("message: gateway returned {status}: {body}"))
            }
            Err(e) => Err(anyhow!("message: failed to reach gateway: {e}")),
        }
    }

    async fn tool_cron(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("cron: `action` required"))?;

        let cron_dir = crate::config::loader::base_dir();
        let cron_path = cron_dir.join("cron").join("jobs.json");

        match action {
            "list" => {
                let jobs = read_cron_jobs(&cron_path).await;
                // Add 1-based index to each job for easier reference by LLMs
                let jobs_with_index: Vec<Value> = jobs
                    .iter()
                    .enumerate()
                    .map(|(i, j)| {
                        let mut indexed = j.clone();
                        indexed["_index"] = json!(i + 1);
                        indexed
                    })
                    .collect();
                Ok(
                    json!({"jobs": jobs_with_index, "hint": "Use index number (#1, #2, etc.) for removal to avoid ID truncation issues"}),
                )
            }
            "add" => {
                let schedule = args["schedule"]
                    .as_str()
                    .ok_or_else(|| anyhow!("cron add: `schedule` required"))?;
                let message = args["message"]
                    .as_str()
                    .ok_or_else(|| anyhow!("cron add: `message` required"))?;
                let name = args["name"].as_str();
                let tz = args["tz"].as_str();
                let agent_id = args["agent_id"].as_str().or(args["agentId"].as_str());

                let mut jobs = read_cron_jobs(&cron_path).await;

                let now_ms = Utc::now().timestamp_millis() as u64;
                let id = Uuid::new_v4().to_string();
                let mut job = json!({
                    "id": id,
                    "agentId": agent_id.unwrap_or("main"),
                    "enabled": true,
                    "createdAtMs": now_ms,
                    "updatedAtMs": now_ms,
                });
                // Schedule: use nested format if tz provided, flat otherwise.
                if let Some(tz_val) = tz {
                    job["schedule"] = json!({"kind": "cron", "expr": schedule, "tz": tz_val});
                } else {
                    job["schedule"] = json!({"kind": "cron", "expr": schedule});
                }
                // Payload in OpenClaw format.
                job["payload"] = json!({"kind": "systemEvent", "text": message});
                if let Some(n) = name {
                    job["name"] = json!(n);
                }

                jobs.push(job);
                write_cron_jobs(&cron_path, &jobs).await?;

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron add: failed to notify gateway reload");
                }

                Ok(json!({"added": id, "schedule": schedule, "message": message}))
            }
            "remove" => {
                let mut jobs = read_cron_jobs(&cron_path).await;

                // Support both `id` and `index` parameters (prefer index for reliability)
                let removed_job = if let Some(index) = args["index"].as_u64() {
                    // 1-based index
                    let idx = index as usize;
                    if idx == 0 || idx > jobs.len() {
                        return Err(anyhow!(
                            "cron remove: invalid index {} (valid: 1-{})",
                            index,
                            jobs.len()
                        ));
                    }
                    let job = jobs.remove(idx - 1);
                    write_cron_jobs(&cron_path, &jobs).await?;
                    job
                } else if let Some(id) = args["id"].as_str() {
                    let before = jobs.len();
                    jobs.retain(|j| j["id"].as_str() != Some(id));
                    let removed = before - jobs.len();
                    if removed == 0 {
                        return Err(anyhow!("cron remove: job not found with id={}", id));
                    }
                    write_cron_jobs(&cron_path, &jobs).await?;
                    json!({"id": id, "count": removed})
                } else {
                    return Err(anyhow!(
                        "cron remove: `index` or `id` required (index is preferred)"
                    ));
                };

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron remove: failed to notify gateway reload");
                }

                Ok(json!({"removed": removed_job}))
            }
            "enable" | "disable" => {
                let enabled = action == "enable";
                let mut jobs = read_cron_jobs(&cron_path).await;

                let idx = if let Some(index) = args["index"].as_u64() {
                    let idx = index as usize;
                    if idx == 0 || idx > jobs.len() {
                        return Err(anyhow!(
                            "cron {}: invalid index {} (valid: 1-{})",
                            action, index, jobs.len()
                        ));
                    }
                    idx - 1
                } else if let Some(id) = args["id"].as_str() {
                    match jobs.iter().position(|j| j["id"].as_str() == Some(id)) {
                        Some(pos) => pos,
                        None => return Err(anyhow!("cron {}: job not found with id={}", action, id)),
                    }
                } else {
                    return Err(anyhow!(
                        "cron {}: `index` or `id` required (index is preferred)",
                        action
                    ));
                };

                let id = jobs[idx]["id"].as_str().unwrap_or("?").to_string();
                jobs[idx]["enabled"] = json!(enabled);
                jobs[idx]["updatedAtMs"] = json!(Utc::now().timestamp_millis() as u64);
                write_cron_jobs(&cron_path, &jobs).await?;

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron {}: failed to notify gateway reload", action);
                }

                Ok(json!({action: id}))
            }
            "edit" => {
                let mut jobs = read_cron_jobs(&cron_path).await;

                let idx = if let Some(index) = args["index"].as_u64() {
                    let idx = index as usize;
                    if idx == 0 || idx > jobs.len() {
                        return Err(anyhow!(
                            "cron edit: invalid index {} (valid: 1-{})",
                            index, jobs.len()
                        ));
                    }
                    idx - 1
                } else if let Some(id) = args["id"].as_str() {
                    match jobs.iter().position(|j| j["id"].as_str() == Some(id)) {
                        Some(pos) => pos,
                        None => return Err(anyhow!("cron edit: job not found with id={}", id)),
                    }
                } else {
                    return Err(anyhow!(
                        "cron edit: `index` or `id` required (index is preferred)"
                    ));
                };

                let id = jobs[idx]["id"].as_str().unwrap_or("?").to_string();
                if let Some(schedule) = args["schedule"].as_str() {
                    let tz = args["tz"].as_str();
                    if let Some(tz_val) = tz {
                        jobs[idx]["schedule"] = json!({"kind": "cron", "expr": schedule, "tz": tz_val});
                    } else {
                        jobs[idx]["schedule"] = json!({"kind": "cron", "expr": schedule});
                    }
                }
                if let Some(message) = args["message"].as_str() {
                    jobs[idx]["payload"] = json!({"kind": "systemEvent", "text": message});
                }
                if let Some(name) = args["name"].as_str() {
                    jobs[idx]["name"] = json!(name);
                }
                if let Some(agent_id) = args["agentId"].as_str().or(args["agent_id"].as_str()) {
                    jobs[idx]["agentId"] = json!(agent_id);
                }
                jobs[idx]["updatedAtMs"] = json!(Utc::now().timestamp_millis() as u64);
                write_cron_jobs(&cron_path, &jobs).await?;

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron edit: failed to notify gateway reload");
                }

                Ok(json!({"edited": id}))
            }
            other => Err(anyhow!(
                "cron: unsupported action `{other}` (list, add, edit, remove, enable, disable)"
            )),
        }
    }
}

/// Read cron jobs from the OpenClaw-compatible jobs.json file.
/// Handles both bare array `[...]` and wrapped `{"version":1,"jobs":[...]}` formats.
async fn read_cron_jobs(path: &std::path::Path) -> Vec<Value> {
    let data = tokio::fs::read_to_string(path)
        .await
        .unwrap_or_else(|_| "[]".to_owned());
    // Try wrapped format first.
    if let Ok(wrapper) = serde_json::from_str::<Value>(&data) {
        if let Some(jobs) = wrapper.get("jobs").and_then(|v| v.as_array()) {
            return jobs.clone();
        }
        // Fall through to try as bare array.
        if let Some(arr) = wrapper.as_array() {
            return arr.clone();
        }
    }
    Vec::new()
}

/// Write cron jobs in OpenClaw-compatible format: {"version":1,"jobs":[...]}.
async fn write_cron_jobs(path: &std::path::Path, jobs: &[Value]) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let wrapper = json!({"version": 1, "jobs": jobs});
    tokio::fs::write(path, serde_json::to_string_pretty(&wrapper)?)
        .await
        .map_err(|e| anyhow!("cron: failed to write jobs: {e}"))?;
    Ok(())
}

impl AgentRuntime {
    async fn tool_sessions_send(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let message = args["message"]
            .as_str()
            .ok_or_else(|| anyhow!("sessions_send: `message` required"))?
            .to_owned();
        let agent_id = args["agentId"]
            .as_str()
            .or_else(|| args["agent_id"].as_str());
        let session_key = args["sessionKey"]
            .as_str()
            .or_else(|| args["session_key"].as_str());

        let registry = self
            .agents
            .as_ref()
            .ok_or_else(|| anyhow!("sessions_send: agent registry not available"))?;

        // Resolve target: if agentId given, send to that agent; otherwise use
        // session_key to find an agent.
        let target_id = agent_id.unwrap_or(&ctx.agent_id);
        let target = registry
            .get(target_id)
            .map_err(|_| anyhow!("sessions_send: agent `{target_id}` not found"))?;

        let child_session = session_key
            .map(|s| s.to_owned())
            .unwrap_or_else(|| format!("{}:send:{}", ctx.session_key, Uuid::new_v4()));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentReply>();
        let msg = AgentMessage {
            session_key: child_session.clone(),
            text: message,
            channel: format!("sessions_send:{}", ctx.agent_id),
            peer_id: ctx.agent_id.clone(),
            chat_id: String::new(),
            reply_tx,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
        };

        target
            .tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("sessions_send: agent `{target_id}` inbox closed"))?;

        let timeout_secs = self
            .config
            .agents
            .defaults
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS as u32) as u64;

        let reply = tokio::time::timeout(Duration::from_secs(timeout_secs), reply_rx)
            .await
            .map_err(|_| anyhow!("sessions_send: timed out after {timeout_secs}s"))?
            .map_err(|_| anyhow!("sessions_send: reply channel dropped"))?;

        Ok(json!({
            "session_key": child_session,
            "agent_id": target_id,
            "reply": reply.text
        }))
    }

    async fn tool_sessions_list(&self) -> Result<Value> {
        let sessions = self.store.db.list_sessions()?;
        let list: Vec<Value> = sessions
            .iter()
            .filter_map(|key| {
                let meta = self.store.db.get_session_meta(key).ok().flatten();
                Some(json!({
                    "session_key": key,
                    "message_count": meta.as_ref().map(|m| m.message_count).unwrap_or(0),
                    "last_active": meta.as_ref().map(|m| m.last_active).unwrap_or(0),
                    "created_at": meta.as_ref().map(|m| m.created_at).unwrap_or(0),
                }))
            })
            .collect();
        Ok(json!({"sessions": list, "count": list.len()}))
    }

    async fn tool_sessions_history(&self, args: Value) -> Result<Value> {
        let session_key = args["sessionKey"]
            .as_str()
            .or_else(|| args["session_key"].as_str())
            .ok_or_else(|| anyhow!("sessions_history: `sessionKey` required"))?;
        let limit = args["limit"].as_u64().unwrap_or(50) as usize;

        let messages = self.store.db.load_messages(session_key)?;
        let total = messages.len();
        let truncated: Vec<&Value> = messages.iter().rev().take(limit).collect();

        Ok(json!({
            "session_key": session_key,
            "messages": truncated,
            "total": total,
            "returned": truncated.len()
        }))
    }

    async fn tool_session_status(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let session_key = args["sessionKey"]
            .as_str()
            .or_else(|| args["session_key"].as_str())
            .unwrap_or(&ctx.session_key);

        let meta = self.store.db.get_session_meta(session_key)?;

        match meta {
            Some(m) => Ok(json!({
                "session_key": session_key,
                "message_count": m.message_count,
                "last_active": m.last_active,
                "created_at": m.created_at,
                "active": true
            })),
            None => Ok(json!({
                "session_key": session_key,
                "active": false,
                "note": "session not found or no metadata"
            })),
        }
    }

    async fn tool_gateway(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("gateway: `action` required"))?;

        let port = self.config.gateway.port;
        let version = env!("CARGO_PKG_VERSION");

        match action {
            "status" | "health" => Ok(json!({
                "status": "running",
                "version": version,
                "port": port,
                "agents": self.agents.as_ref().map(|r| r.all().len()).unwrap_or(0),
            })),
            "version" => Ok(json!({
                "version": version,
                "name": "rsclaw",
            })),
            other => Err(anyhow!(
                "gateway: unsupported action `{other}` (status, health, version)"
            )),
        }
    }

    async fn tool_pairing(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("pairing: `action` required"))?;

        let port = self.config.gateway.port;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}/api/v1");
        let auth_token = self
            .config
            .gateway
            .auth_token
            .as_deref()
            .unwrap_or_default();

        let auth_header = if auth_token.is_empty() {
            String::new()
        } else {
            format!("Bearer {auth_token}")
        };

        match action {
            "list" => {
                let mut req = client.get(format!("{base}/channels/pairings"));
                if !auth_header.is_empty() {
                    req = req.header("Authorization", &auth_header);
                }
                let resp = req.send().await?;
                let data: Value = resp.json().await?;
                Ok(data)
            }
            "approve" => {
                let code = args["code"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pairing approve: `code` required"))?;
                let mut req = client
                    .post(format!("{base}/channels/pair"))
                    .json(&json!({"code": code}));
                if !auth_header.is_empty() {
                    req = req.header("Authorization", &auth_header);
                }
                let resp = req.send().await?;
                let data: Value = resp.json().await?;
                Ok(data)
            }
            "revoke" => {
                let channel = args["channel"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pairing revoke: `channel` required"))?;
                let peer_id = args["peerId"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pairing revoke: `peerId` required"))?;
                let mut req = client
                    .post(format!("{base}/channels/unpair"))
                    .json(&json!({"channel": channel, "peerId": peer_id}));
                if !auth_header.is_empty() {
                    req = req.header("Authorization", &auth_header);
                }
                let resp = req.send().await?;
                let data: Value = resp.json().await?;
                Ok(data)
            }
            other => Err(anyhow!(
                "pairing: unsupported action `{other}` (list, approve, revoke)"
            )),
        }
    }

    async fn tool_doc(&self, args: Value) -> Result<Value> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("doc: `path` required"))?;

        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

        let pb = std::path::PathBuf::from(path_str);
        let full = if pb.is_absolute() { pb } else { workspace.join(path_str) };
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        super::doc::handle(&args, &full).await
    }

    // -------------------------------------------------------------------
    // Consolidated tool handlers
    // -------------------------------------------------------------------

    async fn tool_memory_consolidated(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let action = args["action"].as_str().unwrap_or("search");
        match action {
            "search" => self.tool_memory_search(args).await,
            "get" => self.tool_memory_get(args).await,
            "put" => self.tool_memory_put(ctx, args).await,
            "delete" => self.tool_memory_delete(args).await,
            _ => bail!("memory: unknown action '{action}' (search, get, put, delete)"),
        }
    }

    async fn tool_session_consolidated(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let action = args["action"].as_str().unwrap_or("list");
        match action {
            "send" => self.tool_sessions_send(ctx, args).await,
            "list" => self.tool_sessions_list().await,
            "history" => self.tool_sessions_history(args).await,
            "status" => self.tool_session_status(ctx, args).await,
            _ => bail!("session: unknown action '{action}' (send, list, history, status)"),
        }
    }

    async fn tool_agent_consolidated(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let action = args["action"].as_str().unwrap_or("list");
        match action {
            "spawn" => self.tool_agent_spawn(args).await,
            "task" => self.tool_agent_task(ctx, args).await,
            "send" => self.tool_agent_send(ctx, args).await,
            "list" => self.tool_agent_list().await,
            "kill" => {
                let id = args["id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("agent kill: `id` required"))?;
                Ok(json!({
                    "action": "kill",
                    "id": id,
                    "note": "agent termination not yet implemented; agent will stop on next idle timeout"
                }))
            }
            _ => bail!("agent: unknown action '{action}' (spawn, task, list, kill)"),
        }
    }

    async fn tool_channel_consolidated(&self, args: Value) -> Result<Value> {
        let channel_type = args["channel"].as_str().unwrap_or("unknown").to_owned();
        self.tool_channel_actions(&channel_type, args).await
    }

    async fn tool_channel_actions(&self, channel_type: &str, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("{channel_type}_actions: `action` required"))?;
        let chat_id = args["chatId"]
            .as_str()
            .or_else(|| args["chat_id"].as_str())
            .unwrap_or("");
        let text = args["text"].as_str().unwrap_or("");
        let message_id = args["messageId"]
            .as_str()
            .or_else(|| args["message_id"].as_str())
            .unwrap_or("");

        Ok(json!({
            "channel": channel_type,
            "action": action,
            "chatId": chat_id,
            "text": text,
            "messageId": message_id,
            "status": "stub",
            "note": format!(
                "{channel_type} action `{action}` received. \
                 Channel-specific API integration is not yet wired — \
                 use the `message` tool for basic send operations."
            )
        }))
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Expand a leading `~/` to the user's home directory.
fn expand_tilde(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        dirs_next::home_dir().unwrap_or_default().join(rest)
    } else if p == "~" {
        dirs_next::home_dir().unwrap_or_default()
    } else {
        std::path::PathBuf::from(p)
    }
}

// ---------------------------------------------------------------------------
// File extraction helpers (FileAttachment gate)
// ---------------------------------------------------------------------------

/// Attempt to extract readable text from a file based on extension.
/// Returns `None` for binary/unrecognized formats.
async fn extract_file_text(filename: &str, bytes: &[u8]) -> Option<String> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".pdf") {
        match crate::agent::doc::safe_extract_pdf_from_mem(bytes) {
            Ok(text) => return Some(text),
            Err(_) => {}
        }
        // Fallback to pdftotext CLI
        let tmp = std::env::temp_dir().join(format!("rsclaw_extract_{}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, bytes).ok()?;
        let output = std::process::Command::new("pdftotext")
            .args([tmp.to_str().unwrap_or(""), "-"])
            .output();
        let _ = std::fs::remove_file(&tmp);
        output
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    } else if lower.ends_with(".docx") || lower.ends_with(".xlsx") || lower.ends_with(".pptx") {
        crate::channel::extract_office_text(filename, bytes)
    } else if is_likely_text_file(&lower) {
        Some(String::from_utf8_lossy(bytes).to_string())
    } else if is_audio_or_video(&lower) {
        extract_audio_text(bytes, &lower).await
    } else {
        None
    }
}

fn is_audio_or_video(lower: &str) -> bool {
    [
        ".mp4", ".mov", ".avi", ".mkv", ".webm", ".flv", ".wmv", ".mp3", ".wav", ".ogg", ".m4a",
        ".aac", ".flac", ".wma", ".opus",
    ]
    .iter()
    .any(|e| lower.ends_with(e))
}

/// Extract text from audio/video by running ffmpeg -> whisper.
async fn extract_audio_text(bytes: &[u8], lower_filename: &str) -> Option<String> {
    let ext = lower_filename.rsplit('.').next().unwrap_or("mp4");
    let mime = match ext {
        "mp4" | "m4a" | "m4v" => "video/mp4",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "amr" => "audio/amr",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    };

    tracing::info!(file = %lower_filename, bytes = bytes.len(), "extract_audio_text: starting");

    let client = reqwest::Client::new();
    let result =
        crate::channel::transcription::transcribe_audio(&client, bytes, lower_filename, mime).await;

    match result {
        Ok(text) if !text.trim().is_empty() => Some(format!(
            "[Audio transcription from {ext} file]\n{}",
            text.trim()
        )),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("extract_audio_text: transcription failed: {e:#}");
            None
        }
    }
}

fn is_likely_text_file(lower: &str) -> bool {
    [
        ".txt", ".md", ".csv", ".json", ".toml", ".yaml", ".yml", ".xml", ".html", ".rs", ".py",
        ".js", ".ts", ".go", ".sh", ".log", ".conf", ".cfg", ".c", ".h", ".java", ".css", ".sql",
        ".rb", ".php", ".swift", ".kt", ".lua",
    ]
    .iter()
    .any(|e| lower.ends_with(e))
}

// ---------------------------------------------------------------------------
// Web tool helpers
// ---------------------------------------------------------------------------

/// Search engine URLs from defaults.toml (compile-time embedded).
fn search_engine_url(name: &str) -> &'static str {
    static URLS: std::sync::LazyLock<std::collections::HashMap<String, String>> =
        std::sync::LazyLock::new(|| {
            #[derive(serde::Deserialize)]
            struct Entry {
                name: String,
                url: String,
            }
            #[derive(serde::Deserialize)]
            struct Defs {
                #[serde(default)]
                search_engines: Vec<Entry>,
            }
            let defaults_str = crate::config::loader::load_defaults_toml();
            let defs: Defs = toml::from_str(&defaults_str).unwrap_or(Defs {
                search_engines: vec![],
            });
            defs.search_engines
                .into_iter()
                .map(|e| (e.name, e.url))
                .collect()
        });
    URLS.get(name).map(|s| s.as_str()).unwrap_or("")
}

/// URL-encode a string for use in query parameters.
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 3);
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char);
                }
                _ => {
                    out.push('%');
                    out.push_str(&format!("{byte:02X}"));
                }
            }
        }
        out
    }
}

/// Parse DuckDuckGo HTML search results into structured results.
fn lang_to_bing_mkt(lang: &str) -> &'static str {
    match lang.to_lowercase().as_str() {
        "chinese" | "zh" => "zh-CN",
        "english" | "en" => "en-US",
        "japanese" | "ja" => "ja-JP",
        "korean" | "ko" => "ko-KR",
        "thai" | "th" => "th-TH",
        "vietnamese" | "vi" => "vi-VN",
        "indonesian" | "id" | "bahasa" => "id-ID",
        "malay" | "ms" => "ms-MY",
        "tagalog" | "tl" | "filipino" => "en-PH",
        "burmese" | "my" => "en-US", // Bing has no Burmese market
        "khmer" | "km" => "en-US",   // no Khmer market
        "lao" | "lo" => "en-US",     // no Lao market
        "spanish" | "es" => "es-ES",
        "french" | "fr" => "fr-FR",
        "german" | "de" => "de-DE",
        "portuguese" | "pt" => "pt-BR",
        "russian" | "ru" => "ru-RU",
        "arabic" | "ar" => "ar-SA",
        "hindi" | "hi" => "hi-IN",
        _ => "",
    }
}

fn parse_ddg_results(html: &str, limit: usize) -> Vec<Value> {
    let mut results = Vec::new();

    // Match result links: <a class="result__a" href="...">title</a>
    let link_re =
        regex::Regex::new(r#"<a\s+class="result__a"[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    // Match snippets: <a class="result__snippet"...>snippet</a>
    let snippet_re = regex::Regex::new(r#"<a\s+class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();

    let link_caps: Vec<_> = link_re.captures_iter(html).collect();
    let snippet_caps: Vec<_> = snippet_re.captures_iter(html).collect();

    for (i, cap) in link_caps.iter().enumerate().take(limit) {
        let raw_url = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let snippet = snippet_caps
            .get(i)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");

        // DDG wraps URLs through a redirect; extract the actual URL.
        let url = if let Some(pos) = raw_url.find("uddg=") {
            let start = pos + 5;
            let end = raw_url[start..]
                .find('&')
                .map(|e| start + e)
                .unwrap_or(raw_url.len());
            percent_decode(&raw_url[start..end])
        } else {
            raw_url.to_owned()
        };

        results.push(json!({
            "title": strip_inline_tags(title),
            "url": url,
            "snippet": strip_inline_tags(snippet)
        }));
    }

    results
}

fn parse_bing_html_results(html: &str, limit: usize) -> Vec<Value> {
    let mut results = Vec::new();
    // Bing: <li class="b_algo" ...>...<a href="URL">TITLE</a>...<p>SNIPPET</p>
    // Split by b_algo markers since </li> matching is unreliable
    let parts: Vec<&str> = html.split("class=\"b_algo\"").collect();
    let link_re = regex::Regex::new(r#"<a[^>]*href="(https?://[^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    let snippet_re = regex::Regex::new(r#"<p[^>]*>(.*?)</p>"#).unwrap();

    // Skip first part (before first b_algo)
    for block in parts.iter().skip(1).take(limit) {
        let (url, title) = link_re
            .captures(block)
            .map(|c| {
                (
                    c.get(1).map(|m| m.as_str()).unwrap_or(""),
                    c.get(2).map(|m| m.as_str()).unwrap_or(""),
                )
            })
            .unwrap_or(("", ""));
        let snippet = snippet_re
            .captures(block)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        if !url.is_empty() {
            results.push(json!({
                "title": strip_inline_tags(title),
                "url": url,
                "snippet": strip_inline_tags(snippet)
            }));
        }
    }
    results
}

fn parse_baidu_results(html: &str, limit: usize) -> Vec<Value> {
    let mut results = Vec::new();
    // Baidu: <h3 class="t"><a href="URL">TITLE</a></h3> ... <span
    // class="content-right_...">SNIPPET
    let link_re = regex::Regex::new(r#"<h3[^>]*>\s*<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    let snippet_re =
        regex::Regex::new(r#"<span[^>]*class="content-right[^"]*"[^>]*>(.*?)</span>"#).unwrap();

    let links: Vec<_> = link_re.captures_iter(html).collect();
    let snippets: Vec<_> = snippet_re.captures_iter(html).collect();

    for (i, cap) in links.iter().enumerate().take(limit) {
        let url = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let snippet = snippets
            .get(i)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        if !url.is_empty() {
            results.push(json!({
                "title": strip_inline_tags(title),
                "url": url,
                "snippet": strip_inline_tags(snippet)
            }));
        }
    }
    results
}

fn parse_sogou_results(html: &str, limit: usize) -> Vec<Value> {
    let mut results = Vec::new();
    let link_re = regex::Regex::new(r#"<h3[^>]*>\s*<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    for cap in link_re.captures_iter(html).take(limit) {
        let url = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        if !url.is_empty() {
            results.push(json!({
                "title": strip_inline_tags(title),
                "url": url,
                "snippet": ""
            }));
        }
    }
    results
}

/// Detect if HTML response is a CAPTCHA/verification page.
fn is_captcha_page(html: &str) -> bool {
    let lower = html.to_lowercase();
    lower.contains("captcha") || lower.contains("验证码")
        || lower.contains("人机验证") || lower.contains("verify you are human")
        || lower.contains("robot") || lower.contains("unusual traffic")
        || lower.contains("are you a robot") || lower.contains("security check")
        || lower.contains("challenge-form") || lower.contains("cf-browser-verification")
        || lower.contains("antibot") || lower.contains("recaptcha")
        || lower.contains("hcaptcha") || lower.contains("turnstile")
}

/// Simple percent-decoding for URL extraction.
fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip inline HTML tags (bold, italic, etc.) from a snippet.
fn strip_inline_tags(s: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re.replace_all(s, "");
    decode_html_entities(&text)
}

/// Extract <title> content from HTML.
/// Truncate a string to at most `max` characters (UTF-8 safe).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push_str("\n...(truncated)");
        t
    }
}

fn extract_html_title(html: &str) -> String {
    let re = regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| decode_html_entities(m.as_str().trim()))
        .unwrap_or_default()
}

/// Strip all HTML to plain text.
fn strip_html(html: &str) -> String {
    // Remove script and style blocks.
    let no_script = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>")
        .unwrap()
        .replace_all(html, "");
    let no_style = regex::Regex::new(r"(?is)<style[^>]*>.*?</style>")
        .unwrap()
        .replace_all(&no_script, "");
    // Remove all remaining tags.
    let no_tags = regex::Regex::new(r"<[^>]+>")
        .unwrap()
        .replace_all(&no_style, " ");
    // Decode entities.
    let decoded = decode_html_entities(&no_tags);
    // Collapse whitespace.
    regex::Regex::new(r"\s+")
        .unwrap()
        .replace_all(&decoded, " ")
        .trim()
        .to_owned()
}

/// Decode common HTML entities.
fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// Format a tool call result as human-readable markdown.
fn format_tool_result(val: &serde_json::Value) -> String {
    // exec tool: { exit_code, stdout, stderr }
    if val.get("stdout").is_some() || val.get("stderr").is_some() {
        let stdout = val["stdout"].as_str().unwrap_or("").trim();
        let stderr = val["stderr"].as_str().unwrap_or("").trim();
        let exit_code = val["exit_code"].as_i64();
        let mut out = String::new();
        if !stdout.is_empty() {
            out.push_str(stdout);
        }
        if !stderr.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[stderr] ");
            out.push_str(stderr);
        }
        if let Some(code) = exit_code {
            if code != 0 {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[exit code: {code}]"));
            }
        }
        if out.is_empty() {
            "(no output)".to_owned()
        } else {
            out
        }
    }
    // read tool: { content, path }
    else if let Some(content) = val.get("content").and_then(|v| v.as_str()) {
        let path = val.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            content.to_owned()
        } else {
            format!("[{path}]\n{content}")
        }
    }
    // web_browser snapshot/action: { action, text }
    else if val.get("action").is_some() && val.get("text").is_some() {
        let action = val["action"].as_str().unwrap_or("");
        let text = val["text"].as_str().unwrap_or("");
        if text.is_empty() {
            format!("[{action}] done")
        } else {
            text.to_owned()
        }
    }
    // web_search: { results: [...] }
    else if let Some(results) = val.get("results").and_then(|v| v.as_array()) {
        let mut out = String::new();
        for (i, r) in results.iter().enumerate() {
            let title = r
                .get("title")
                .or_else(|| r.get("summary"))
                .or_else(|| r.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("(no title)");
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = r.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
            out.push_str(&format!("{}. {}\n", i + 1, title));
            if !url.is_empty() {
                out.push_str(&format!("   {url}\n"));
            }
            if !snippet.is_empty() {
                out.push_str(&format!("   {snippet}\n"));
            }
            out.push('\n');
        }
        if out.is_empty() {
            "No results found. Do NOT retry the same search. Try different keywords or inform the user that no results were found.".to_owned()
        } else {
            out.trim_end().to_owned()
        }
    }
    // cookies: { cookies: [...] }
    else if let Some(cookies) = val.get("cookies").and_then(|v| v.as_array()) {
        let mut out = String::new();
        for c in cookies {
            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("-");
            let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("-");
            let domain = c.get("domain").and_then(|v| v.as_str()).unwrap_or("-");
            let val_short = if value.len() > 30 {
                let end = value
                    .char_indices()
                    .nth(27)
                    .map(|(i, _)| i)
                    .unwrap_or(value.len());
                &value[..end]
            } else {
                value
            };
            out.push_str(&format!("{name}={val_short} ({domain})\n"));
        }
        if out.is_empty() {
            "(no cookies)".to_owned()
        } else {
            out.trim_end().to_owned()
        }
    }
    // Fallback: compact JSON
    else {
        serde_json::to_string_pretty(val).unwrap_or_default()
    }
}

/// Write a dot-path value to the config file (e.g.
/// "tools.upload.max_file_size").
fn write_config_value(dot_path: &str, value: serde_json::Value) -> anyhow::Result<()> {
    use crate::cmd::config_json::{load_config_json, set_nested_value};

    let (path, mut val) = load_config_json()?;

    // Ensure intermediate objects exist
    let parts: Vec<&str> = dot_path.split('.').collect();
    for i in 0..parts.len().saturating_sub(1) {
        let key = parts[i];
        if val.get(key).is_none() {
            val.as_object_mut()
                .map(|o| o.insert(key.to_string(), serde_json::json!({})));
        }
        // Recurse for nested paths
        if i > 0 {
            let prefix = parts[..=i].join(".");
            if crate::cmd::config_json::get_nested_value(&val, &prefix).is_none() {
                set_nested_value(&mut val, &prefix, serde_json::json!({}))?;
            }
        }
    }

    set_nested_value(&mut val, dot_path, value)?;
    std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
    Ok(())
}

/// Auto-detect if a graphical display is available.
/// macOS/Windows always have one; Linux checks DISPLAY/WAYLAND_DISPLAY.
fn has_display() -> bool {
    if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
        true
    } else {
        std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok()
    }
}

/// Detect Chrome/Chromium binary path.
/// Priority: ~/.rsclaw/tools/ > system PATH > well-known locations.
fn detect_chrome() -> Option<String> {
    // 1. Check locally installed via `rsclaw tools install chrome` (highest priority)
    let tools_dir = crate::config::loader::base_dir().join("tools/chrome");
    if tools_dir.exists() {
        #[cfg(target_os = "windows")]
        let bin = tools_dir.join("chrome.exe");
        #[cfg(target_os = "macos")]
        let bin = tools_dir.join("Chromium.app/Contents/MacOS/Chromium");
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let bin = tools_dir.join("chrome");

        if bin.exists() {
            return Some(bin.to_string_lossy().to_string());
        }
    }

    // 2. System well-known locations
    #[cfg(target_os = "macos")]
    {
        let app_path = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
        if std::path::Path::new(app_path).exists() {
            return Some(app_path.to_owned());
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Try Windows registry first (most reliable).
        for key_path in &[
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe",
            r"SOFTWARE\Wow6432Node\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe",
        ] {
            if let Ok(output) = std::process::Command::new("reg")
                .args(["query", &format!(r"HKLM\{key_path}"), "/ve"])
                .output()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                // Parse "REG_SZ    C:\...\chrome.exe" from output
                if let Some(line) = stdout.lines().find(|l| l.contains("REG_SZ")) {
                    if let Some(path_str) = line.split("REG_SZ").nth(1) {
                        let path_str = path_str.trim();
                        if std::path::Path::new(path_str).exists() {
                            return Some(path_str.to_owned());
                        }
                    }
                }
            }
            // Also try HKCU (per-user installs)
            if let Ok(output) = std::process::Command::new("reg")
                .args(["query", &format!(r"HKCU\{key_path}"), "/ve"])
                .output()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(line) = stdout.lines().find(|l| l.contains("REG_SZ")) {
                    if let Some(path_str) = line.split("REG_SZ").nth(1) {
                        let path_str = path_str.trim();
                        if std::path::Path::new(path_str).exists() {
                            return Some(path_str.to_owned());
                        }
                    }
                }
            }
        }
        // Fallback: well-known paths.
        let candidates = [
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ];
        for path in &candidates {
            if std::path::Path::new(path).exists() {
                return Some(path.to_string());
            }
        }
        if let Ok(userprofile) = std::env::var("USERPROFILE") {
            let user_chrome = format!(
                r"{}\AppData\Local\Google\Chrome\Application\chrome.exe",
                userprofile
            );
            if std::path::Path::new(&user_chrome).exists() {
                return Some(user_chrome);
            }
        }
    }

    // 3. Search PATH
    for name in &["google-chrome", "chromium", "chromium-browser", "chrome"] {
        if let Ok(path) = which::which(name) {
            return Some(path.to_string_lossy().to_string());
        }
    }

    None
}

/// Run a subprocess and return an error if it fails.
async fn run_subprocess(cmd: &str, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow!("{cmd}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("{cmd} failed: {stderr}"));
    }
    Ok(())
}

/// Parse JPEG dimensions from SOF0/SOF2 marker (no external deps).
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        i += 2;
        // SOF0 (0xC0) or SOF2 (0xC2) contain dimensions
        if marker == 0xC0 || marker == 0xC2 {
            if i + 7 <= data.len() {
                let h = u16::from_be_bytes([data[i + 3], data[i + 4]]) as u32;
                let w = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                return Some((w, h));
            }
            return None;
        }
        // Skip segment
        if marker >= 0xC0 && marker != 0xD8 && marker != 0xD9 && marker != 0x00 {
            if i + 2 <= data.len() {
                let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
                i += len;
            } else {
                break;
            }
        }
    }
    None
}

/// Run a PowerShell snippet with the required assemblies pre-loaded.
/// Used for Windows computer_use actions (mouse, keyboard).
async fn run_powershell_input(script: &str) -> Result<()> {
    let full = format!("Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; {script}");
    run_subprocess("powershell", &["-NoProfile", "-Command", &full]).await
}

/// Windows: set cursor position via .NET
async fn win_set_cursor(x: i64, y: i64) -> Result<()> {
    run_powershell_input(&format!(
        "[System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({x},{y})"
    )).await
}

/// Windows: mouse click with P/Invoke. Supports left/right/middle and repeat count.
async fn win_mouse_click(x: i64, y: i64, button: &str, clicks: i32) -> Result<()> {
    let (down_flag, up_flag) = match button {
        "right" => ("0x0008", "0x0010"),
        "middle" => ("0x0020", "0x0040"),
        _ => ("0x0002", "0x0004"),
    };
    run_powershell_input(&format!(
        r#"Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinClick {{
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern void mouse_event(uint f, uint dx, uint dy, uint d, int e);
    public static void Click(int x, int y, uint down, uint up, int n) {{
        SetCursorPos(x, y);
        for (int i = 0; i < n; i++) {{
            mouse_event(down, 0, 0, 0, 0);
            mouse_event(up, 0, 0, 0, 0);
            if (i < n - 1) System.Threading.Thread.Sleep(50);
        }}
    }}
}}
"@
[WinClick]::Click({x}, {y}, {down_flag}, {up_flag}, {clicks})"#
    )).await
}

/// Windows: map key names to SendKeys format, including modifier combos.
fn win_map_key(key: &str) -> String {
    // Handle modifier combos like "ctrl+c" → "^c"
    if key.contains('+') {
        let parts: Vec<&str> = key.split('+').collect();
        let mut prefix = String::new();
        for &modifier in &parts[..parts.len() - 1] {
            match modifier.to_lowercase().as_str() {
                "ctrl" | "control" => prefix.push('^'),
                "alt" => prefix.push('%'),
                "shift" => prefix.push('+'),
                _ => {}
            }
        }
        let base = win_map_single_key(parts[parts.len() - 1]);
        format!("{prefix}{base}")
    } else {
        win_map_single_key(key)
    }
}

/// Check if a key name is a cliclick kp: special key.
fn is_cliclick_special_key(key: &str) -> bool {
    matches!(key.to_lowercase().as_str(),
        "arrow-down" | "arrow-left" | "arrow-right" | "arrow-up"
        | "brightness-down" | "brightness-up"
        | "delete" | "end" | "enter" | "esc"
        | "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8"
        | "f9" | "f10" | "f11" | "f12" | "f13" | "f14" | "f15" | "f16"
        | "fwd-delete" | "home"
        | "keys-light-down" | "keys-light-toggle" | "keys-light-up"
        | "mute" | "num-0" | "num-1" | "num-2" | "num-3" | "num-4"
        | "num-5" | "num-6" | "num-7" | "num-8" | "num-9"
        | "num-clear" | "num-divide" | "num-enter" | "num-equals"
        | "num-minus" | "num-multiply" | "num-plus"
        | "page-down" | "page-up"
        | "play-next" | "play-pause" | "play-previous"
        | "return" | "space" | "tab"
        | "volume-down" | "volume-up"
    )
}

/// Map modifier name to cliclick/xdotool format. Returns owned String to avoid lifetime issues.
fn map_modifier(name: &str) -> String {
    match name.to_lowercase().as_str() {
        "ctrl" | "control" => "ctrl".to_owned(),
        "alt" | "option" => "alt".to_owned(),
        "shift" => "shift".to_owned(),
        "cmd" | "command" | "super" => "cmd".to_owned(),
        _ => name.to_owned(),
    }
}

/// Map modifier name to xdotool format.
fn map_modifier_xdotool(name: &str) -> String {
    match name.to_lowercase().as_str() {
        "ctrl" | "control" => "ctrl".to_owned(),
        "alt" | "option" => "alt".to_owned(),
        "shift" => "shift".to_owned(),
        "super" | "cmd" | "command" => "super".to_owned(),
        _ => name.to_owned(),
    }
}

fn win_map_single_key(key: &str) -> String {
    match key {
        "Return" | "Enter" => "{ENTER}".to_owned(),
        "Escape" | "Esc" => "{ESC}".to_owned(),
        "Tab" => "{TAB}".to_owned(),
        "BackSpace" | "Backspace" => "{BACKSPACE}".to_owned(),
        "Delete" => "{DELETE}".to_owned(),
        "Insert" => "{INSERT}".to_owned(),
        "Up" => "{UP}".to_owned(),
        "Down" => "{DOWN}".to_owned(),
        "Left" => "{LEFT}".to_owned(),
        "Right" => "{RIGHT}".to_owned(),
        "Home" => "{HOME}".to_owned(),
        "End" => "{END}".to_owned(),
        "Page_Up" | "PageUp" => "{PGUP}".to_owned(),
        "Page_Down" | "PageDown" => "{PGDN}".to_owned(),
        "space" => " ".to_owned(),
        "F1" => "{F1}".to_owned(),
        "F2" => "{F2}".to_owned(),
        "F3" => "{F3}".to_owned(),
        "F4" => "{F4}".to_owned(),
        "F5" => "{F5}".to_owned(),
        "F6" => "{F6}".to_owned(),
        "F7" => "{F7}".to_owned(),
        "F8" => "{F8}".to_owned(),
        "F9" => "{F9}".to_owned(),
        "F10" => "{F10}".to_owned(),
        "F11" => "{F11}".to_owned(),
        "F12" => "{F12}".to_owned(),
        "Print" | "PrintScreen" => "{PRTSC}".to_owned(),
        "Scroll_Lock" | "ScrollLock" => "{SCROLLLOCK}".to_owned(),
        "Pause" | "Break" => "{BREAK}".to_owned(),
        "Caps_Lock" | "CapsLock" => "{CAPSLOCK}".to_owned(),
        "Num_Lock" | "NumLock" => "{NUMLOCK}".to_owned(),
        other => other.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Help text builder
// ---------------------------------------------------------------------------

/// Compute the set of allowed tool names based on toolset level + custom tools.
/// Returns None for "full" (no filtering), Some(set) for others.
fn toolset_allowed_names(
    toolset: &str,
    custom_tools: Option<&Vec<String>>,
) -> Option<std::collections::HashSet<String>> {
    const MINIMAL: &[&str] = &["execute_command", "read_file", "write_file", "send_file", "list_dir", "search_file", "search_content", "web_search", "web_fetch", "memory"];
    const WEB: &[&str] = &["web_search", "web_fetch", "web_browser", "web_download", "read_file", "write_file", "list_dir", "search_file", "memory"];
    const CODE: &[&str] = &["execute_command", "read_file", "write_file", "list_dir", "search_file", "search_content", "memory"];
    const STANDARD: &[&str] = &[
        "execute_command",
        "read_file",
        "write_file",
        "list_dir",
        "search_file",
        "search_content",
        "web_search",
        "web_fetch",
        "memory",
        "web_browser",
        "image_gen",
        "channel",
        "cron",
        "computer_use",
    ];

    let base: Option<&[&str]> = match toolset {
        "minimal" => Some(MINIMAL),
        "web" => Some(WEB),
        "code" => Some(CODE),
        "standard" => Some(STANDARD),
        "full" => None,
        _ => Some(STANDARD),
    };

    match (base, custom_tools) {
        (None, None) => None, // full, no custom -> no filtering
        (None, Some(extra)) => {
            // full + custom whitelist -> use custom as whitelist
            Some(extra.iter().cloned().collect())
        }
        (Some(base_list), None) => Some(base_list.iter().map(|s| s.to_string()).collect()),
        (Some(base_list), Some(extra)) => {
            // Merge: toolset base + custom extras, deduplicated
            let mut set: std::collections::HashSet<String> =
                base_list.iter().map(|s| s.to_string()).collect();
            set.extend(extra.iter().cloned());
            Some(set)
        }
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m {s}s")
    } else if mins > 0 {
        format!("{mins}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn build_help_text_filtered(allowed: &str, lang: &str) -> String {
    let full = allowed == "*";
    let zh = lang == "zh";
    let has = |cmd: &str| -> bool {
        if full { return true; }
        READONLY_COMMANDS.iter().any(|c| *c == cmd) || allowed.split('|').any(|a| a.trim() == cmd)
    };

    let mut h = String::from(if zh { "可用命令：\n\n" } else { "Available commands:\n\n" });

    if has("/run") || has("/find") || has("/grep") {
        h.push_str(if zh { "终端：\n" } else { "Shell:\n" });
        if has("/run") {
            h.push_str(if zh { "  /run <命令>       执行终端命令\n  $ <命令>           执行终端命令（快捷方式）\n" } else { "  /run <cmd>        Execute a shell command\n  $ <cmd>           Execute a shell command (shortcut)\n" });
        }
        if has("/find") { h.push_str(if zh { "  /find <模式>      按名称查找文件\n" } else { "  /find <pattern>   Find files by name\n" }); }
        if has("/grep") { h.push_str(if zh { "  /grep <模式>      搜索文件内容\n" } else { "  /grep <pattern>   Search file contents\n" }); }
        h.push('\n');
    }

    if has("/read") || has("/write") || has("/ls") {
        h.push_str(if zh { "文件：\n" } else { "Files:\n" });
        if has("/read") { h.push_str(if zh { "  /read <路径>      读取文件\n" } else { "  /read <path>      Read a file\n" }); }
        if has("/write") { h.push_str(if zh { "  /write <路径> <内容>  写入文件\n" } else { "  /write <path> <content>  Write to a file\n" }); }
        if has("/ls") { h.push_str(if zh { "  /ls [路径]        列出目录\n" } else { "  /ls [path]        List directory\n" }); }
        h.push('\n');
    }

    if has("/search") || has("/fetch") || has("/screenshot") || has("/ss") {
        h.push_str(if zh { "搜索与网页：\n" } else { "Search & Web:\n" });
        if has("/search") { h.push_str(if zh { "  /search <关键词>  搜索网页\n" } else { "  /search <query>   Search the web\n" }); }
        if has("/fetch") { h.push_str(if zh { "  /fetch <网址>     抓取网页内容\n" } else { "  /fetch <url>      Fetch a web page\n" }); }
        if has("/screenshot") { h.push_str(if zh { "  /screenshot <网址> 网页截图\n" } else { "  /screenshot <url> Screenshot a web page\n" }); }
        if has("/ss") { h.push_str(if zh { "  /ss               桌面截图\n" } else { "  /ss               Screenshot desktop\n" }); }
        h.push('\n');
    }

    if has("/remember") || has("/recall") {
        h.push_str(if zh { "记忆：\n" } else { "Memory:\n" });
        if has("/remember") { h.push_str(if zh { "  /remember <文本>  保存到记忆\n" } else { "  /remember <text>  Save to memory\n" }); }
        if has("/recall") { h.push_str(if zh { "  /recall <关键词>  搜索记忆\n" } else { "  /recall <query>   Search memory\n" }); }
        h.push('\n');
    }

    h.push_str(if zh { "背景上下文：\n" } else { "Background Context:\n" });
    h.push_str(if zh { "  /ctx <文本>              添加持久上下文\n" } else { "  /ctx <text>              Add persistent context\n" });
    h.push_str(if zh { "  /ctx --ttl <N> <文本>    添加上下文（N轮后过期）\n" } else { "  /ctx --ttl <N> <text>    Add context (expires in N turns)\n" });
    if full { h.push_str(if zh { "  /ctx --global <文本>     添加全局上下文\n" } else { "  /ctx --global <text>     Add global context (all sessions)\n" }); }
    h.push_str(if zh { "  /ctx --list              列出活跃上下文\n" } else { "  /ctx --list              List active context entries\n" });
    h.push_str(if zh { "  /ctx --remove <id>       移除指定上下文\n" } else { "  /ctx --remove <id>       Remove entry by id\n" });
    h.push_str(if zh { "  /ctx --clear             清除当前会话所有上下文\n" } else { "  /ctx --clear             Clear all context for this session\n" });
    h.push('\n');

    h.push_str(if zh { "快速提问：\n" } else { "Side Query:\n" });
    h.push_str(if zh { "  /btw <问题>              快速查询（不调用工具）\n" } else { "  /btw <question>          Quick query (no tools, ephemeral)\n" });
    h.push('\n');

    if full {
        h.push_str(if zh { "工具（聚合）：\n" } else { "Tools (consolidated):\n" });
        h.push_str(if zh { "  memory   搜索/获取/保存/删除长期记忆\n" } else { "  memory   search/get/put/delete long-term memory\n" });
        h.push_str(if zh { "  session  发送/列表/历史/状态\n" } else { "  session  send/list/history/status for sessions\n" });
        h.push_str(if zh { "  agent    创建/任务/列表/终止子智能体\n" } else { "  agent    spawn/task/list/kill sub-agents\n" });
        h.push_str(if zh { "  channel  发送/回复/置顶/删除跨渠道消息\n" } else { "  channel  send/reply/pin/delete across channels\n" });
        h.push('\n');
    }

    h.push_str(if zh { "系统：\n" } else { "System:\n" });
    h.push_str(if zh { "  /status           网关状态\n" } else { "  /status           Gateway status\n" });
    h.push_str(if zh { "  /version          查看版本\n" } else { "  /version          Show version\n" });
    h.push_str(if zh { "  /models           列出模型\n" } else { "  /models           List models\n" });
    if has("/model") { h.push_str(if zh { "  /model <名称>     切换模型\n" } else { "  /model <name>     Switch model\n" }); }
    h.push_str(if zh { "  /uptime           查看运行时长\n" } else { "  /uptime           Show uptime\n" });
    h.push('\n');

    h.push_str(if zh { "会话：\n" } else { "Session:\n" });
    h.push_str(if zh { "  /clear            清除会话\n" } else { "  /clear            Clear session\n" });
    h.push_str(if zh { "  /compact          压缩会话并保存记忆\n" } else { "  /compact          Compact session & save to memory\n" });
    h.push_str(if zh { "  /abort            终止当前任务\n" } else { "  /abort            Abort running task\n" });
    if has("/reset") { h.push_str(if zh { "  /reset            重置会话\n" } else { "  /reset            Reset session\n" }); }
    h.push_str(if zh { "  /voice            语音回复模式\n" } else { "  /voice            Voice reply mode\n" });
    h.push_str(if zh { "  /text             文字回复模式\n" } else { "  /text             Text reply mode\n" });
    h.push_str(if zh { "  /history [n]      查看历史\n" } else { "  /history [n]      Show history\n" });
    if has("/sessions") { h.push_str(if zh { "  /sessions         列出会话\n" } else { "  /sessions         List sessions\n" }); }
    h.push('\n');

    h.push_str(if zh { "定时任务：\n" } else { "Cron:\n" });
    h.push_str(if zh { "  /cron list        列出定时任务\n" } else { "  /cron list        List cron jobs\n" });
    h.push('\n');

    if has("/send") {
        h.push_str(if zh { "消息：\n" } else { "Messaging:\n" });
        h.push_str(if zh { "  /send <目标> <消息>  发送消息\n" } else { "  /send <target> <msg>  Send a message\n" });
        h.push('\n');
    }

    if has("/skill") {
        h.push_str(if zh { "技能：\n" } else { "Skill:\n" });
        h.push_str("  /skill install <name>\n  /skill list\n  /skill search <query>\n");
        h.push('\n');
    }

    if full {
        h.push_str(if zh { "上传限制：\n" } else { "Upload & Limits:\n" });
        h.push_str(if zh {
            "  /get_upload_size           查看上传大小限制\n  /set_upload_size <MB>      设置大小限制\n  /get_upload_chars          查看文本字符限制\n  /set_upload_chars <N>      设置字符限制\n  /config_upload_size <MB>   持久化大小限制\n  /config_upload_chars <N>   持久化字符限制\n"
        } else {
            "  /get_upload_size           Show upload size limit\n  /set_upload_size <MB>      Set size limit (runtime)\n  /get_upload_chars          Show text char limit\n  /set_upload_chars <N>      Set char limit (runtime)\n  /config_upload_size <MB>   Set size limit (persistent)\n  /config_upload_chars <N>   Set char limit (persistent)\n"
        });
        h.push('\n');
    }

    h.push_str(if zh { "直接输入消息即可与AI对话。" } else { "Type any message without / to chat with the AI agent." });
    h
}

// ---------------------------------------------------------------------------
// System prompt builder
// ---------------------------------------------------------------------------

/// Build the base system prompt shared by main agent and sub agents.
///
/// Contains: date/time, language, platform, command safety rules.
/// Sub agents call this directly; the main agent calls `build_system_prompt`
/// which adds workspace context, skills, and tool guidance on top.
fn build_base_system_prompt(config: &crate::config::schema::Config) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();

    // Current date/time so the model knows "today", "last Friday", etc.
    let now = chrono::Local::now();
    use chrono::Datelike;
    let weekday = now.date_naive().weekday().num_days_from_monday();
    let last_friday = if weekday >= 4 {
        now.date_naive() - chrono::Duration::days((weekday - 4) as i64)
    } else {
        now.date_naive() - chrono::Duration::days((weekday + 3) as i64)
    };
    let yesterday = now.date_naive() - chrono::Duration::days(1);
    let mut date_line = format!(
        "Current date: {} ({}). Yesterday: {}. Last Friday: {}.",
        now.format("%Y-%m-%d %H:%M"),
        now.format("%A"),
        yesterday.format("%Y-%m-%d"),
        last_friday.format("%Y-%m-%d"),
    );
    if let Some(lang) = config.gateway.as_ref().and_then(|g| g.language.as_deref()) {
        date_line.push_str(&format!(
            "\nDefault response language: {lang}. Always reply in {lang} unless the user explicitly uses another language."
        ));
    }
    parts.push(date_line);

    // Platform information so LLM generates correct shell commands.
    let platform_info = if cfg!(target_os = "windows") {
        "Platform: Windows. Shell: PowerShell. \
         Use PowerShell commands: Get-ChildItem (or dir), Get-Content, Get-Date, Select-Object -Last N (tail). \
         Pipes and filters work naturally: | Where-Object, | Select-Object, | Sort-Object. \
         Paths: backslash or forward slash both work. \
         Examples: Get-Date -Format 'yyyy-MM-dd'; Get-ChildItem | Select-Object -Last 5; Get-Content file.txt."
    } else if cfg!(target_os = "macos") {
        "Platform: macOS. Shell: bash/zsh. Standard Unix commands available (ls, cat, grep, tail, date)."
    } else {
        "Platform: Linux. Shell: bash/sh. Standard Unix commands available (ls, cat, grep, tail, date)."
    };
    parts.push(platform_info.to_string());

    // Windows command safety rules (only on Windows builds).
    if cfg!(target_os = "windows") {
        parts.push(
            "<windows_command_safety>\n\
             Windows command safety rules (ALL mandatory):\n\
             1. Do not wrap a command in an extra shell layer such as `cmd /c`, `powershell -Command`, or `pwsh -Command` unless strictly necessary.\n\
             2. For destructive file operations, only use a fully specified absolute path.\n\
             3. Never generate a command whose quoting, escaping, or trailing backslashes could cause the target path to be truncated or reinterpreted.\n\
             4. Any destructive operation outside the workspace requires explicit user approval.\n\
             5. If a destructive command fails, do NOT retry with workarounds or alternate commands. Stop, explain the failure, and ask the user.\n\
             </windows_command_safety>"
                .to_owned(),
        );
    }

    // Agent loop guidance (helps small models understand the iteration pattern).
    parts.push(
        "<agent_loop>\n\
         You are operating in an agent loop:\n\
         1. Analyze: understand the user's intent and current state\n\
         2. Plan: decide which tool to use next\n\
         3. Execute: call the tool\n\
         4. Observe: check the result\n\
         5. Iterate: repeat until the task is complete, then reply to the user\n\
         If a tool call fails, do NOT retry with the same arguments. Try a different approach or inform the user.\n\
         Never fabricate URLs, file paths, or numeric values.\n\
         When you need a Unix timestamp, use a shell command (e.g. `date +%s`) — never calculate it yourself.\n\
         </agent_loop>"
            .to_owned(),
    );

    parts
}

/// Build the full system prompt for the main agent (base + workspace + skills + tools).
fn build_system_prompt(
    ws_ctx: &WorkspaceContext,
    skills: &SkillRegistry,
    config: &crate::config::schema::Config,
) -> String {
    let mut parts = build_base_system_prompt(config);

    // Tool usage guidance
    {
        parts.push(
            "## Tool Usage Guidelines\n\
             ### File Operations (use dedicated tools, NOT execute_command)\n\
             - List directory contents: use `list_dir` (NOT execute_command ls/dir)\n\
             - Find files by name: use `search_file` (NOT execute_command find)\n\
             - Search file contents: use `search_content` (NOT execute_command grep)\n\
             - Read file: use `read_file`. Write/create file: use `write_file`.\n\
             - For documents (xlsx/docx/pdf/pptx): use the `doc` tool, not execute_command.\n\
             - Reserve `execute_command` for system commands and tasks that have no dedicated tool.\n\
             ### Web Operations\n\
             - When user asks to go to a specific site (e.g. 'go to douyin', 'open taobao'), use `web_browser` directly. Do NOT search first.\n\
             - For general questions or info lookup, use `web_search` first.\n\
             - To download files/images/videos: use `web_download` (supports resume, browser cookies). Do NOT use exec curl/wget.\n\
             - `web_download` path is relative to workspace/downloads/. Just pass the filename like `video.mp4` or `subdir/file.pdf`. Do NOT include `~/`, `~/Downloads/`, or absolute paths.\n\
             - After downloading, use `send_file` to send the file to the user.\n\
             ### Agent & Task Delegation\n\
             You are the architect. Delegate work to sub-agents, never block.\n\
             - Use `agent` action=task for one-shot sub-tasks. Tasks run in the background and results appear on your next turn.\n\
             - Always specify a `toolset` matching the task (web=search/browse, code=read/write/exec, minimal=basic).\n\
             - Give each task a clear, specific `system` role and `message` instruction.\n\
             #### When to delegate\n\
             - Tasks that are independent of each other (e.g. search 3 different topics) -> dispatch ALL at once in parallel.\n\
             - Time-consuming work (web research, file processing, code generation) -> delegate so you can continue talking to the user.\n\
             - Do NOT delegate trivial tasks (simple answers, one read, one search) — do those yourself.\n\
             #### Pipeline pattern (A's output feeds B)\n\
             - Step 1: Dispatch all independent tasks in parallel.\n\
             - Step 2: On your next turn, collect results from [async task completed] messages.\n\
             - Step 3: If further work depends on those results, dispatch new tasks with the collected data.\n\
             - Step 4: Synthesize final results and reply to the user.\n\
             #### Error handling\n\
             - If a task times out, try with a simpler scope or do it yourself.\n\
             - If a task returns an error, explain to the user and offer alternatives.\n\
             ### Other\n\
             - For cron jobs: use the `cron` tool (action=list/add/remove).\n\
             - To install tools (python, node, ffmpeg, chrome, opencode, claude-code, sherpa-onnx): use `install_tool`. Do NOT download/install manually.\n\
             - When user asks about previous conversations, tasks, or anything you don't have context for, use `memory` to recall relevant information before answering.\n\
             - At the start of a new session, if the user's first message references prior work, search memory first."
                .to_owned(),
        );

        // Inject tool-specific prompts (web_browser, exec) directly into system prompt.
        let base = crate::config::loader::base_dir();
        let lang = config.gateway.as_ref().and_then(|g| g.language.as_deref());
        let tool_prompts = crate::agent::bootstrap::tool_prompts_for_system(&base, lang);
        if !tool_prompts.is_empty() {
            parts.push(tool_prompts);
        }

        parts.push(
            "## Self-Evolution — Auto Skill Creation\n\
             When you notice a task pattern repeating (>=3 similar requests), package it as a standard skill after completing the task:\n\
             1. Create SKILL.md in workspace/skills/<slug>/ (keep it under 100 lines)\n\
             2. Frontmatter: name, description, version (no extra fields like author/tags/category)\n\
             3. Body: trigger conditions + key execution steps only (no verbose examples, error tables, or version history)\n\
             4. Record in memory (memory_put) to avoid duplicates\n\
             5. Inform the user\n\
             IMPORTANT: Skills must be concise. Only list the essential steps, not every possible detail."
                .to_owned(),
        );
    }

    // Workspace files segment.
    let ws_segment = ws_ctx.to_prompt_segment();
    if !ws_segment.is_empty() {
        parts.push(ws_segment);
    }

    // Available skills — name + short description. Full prompts injected on-demand.
    if !skills.is_empty() {
        let lines: Vec<_> = skills
            .all()
            .map(|s| {
                format!("- {}", s.name)
            })
            .collect();
        if !lines.is_empty() {
            parts.push(format!("Available skills:\n{}", lines.join("\n")));
        }
    }

    parts.join("\n\n")
}

// ---------------------------------------------------------------------------
// Memory age label — human-readable relative time for staleness awareness
// ---------------------------------------------------------------------------

/// Return a relative time label for memory recall.
/// LLMs can't do date arithmetic, so we use relative descriptions.
fn memory_age_label(now_ts: i64, created_at: i64) -> String {
    let age_secs = (now_ts - created_at).max(0);
    let days = age_secs / 86400;
    match days {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        2..=6 => format!("{days} days ago"),
        7..=13 => "~1 week ago".to_owned(),
        14..=29 => format!("{} weeks ago", days / 7),
        30..=59 => "~1 month ago — may be outdated, verify before using".to_owned(),
        60..=364 => format!("{} months ago — may be outdated, verify before using", days / 30),
        365..=729 => "~1 year ago — likely outdated, verify before using".to_owned(),
        _ => format!("~{} years ago — likely outdated, verify before using", days / 365),
    }
}

// ---------------------------------------------------------------------------
// Hybrid memory retrieval — Reciprocal Rank Fusion
// ---------------------------------------------------------------------------

/// Merge vector-search hits and BM25 hits using Reciprocal Rank Fusion (k=60).
///
/// Documents appearing in both lists get a higher combined score.
/// Documents only in one list still contribute their single-list score.
/// Returns the top `top_k` results as `MemoryDoc`s.
fn rrf_fuse(
    vec_hits: Vec<crate::agent::memory::MemoryDoc>,
    bm25_hits: Vec<crate::store::search::IndexDoc>,
    top_k: usize,
) -> Vec<crate::agent::memory::MemoryDoc> {
    use std::collections::HashMap;

    use crate::agent::memory::MemoryDoc;

    const K: f32 = 60.0;

    // score_map: doc_id → (rrf_score, MemoryDoc)
    let mut scores: HashMap<String, (f32, MemoryDoc)> = HashMap::new();

    // Vector hits — rank 1-based.
    for (rank, doc) in vec_hits.into_iter().enumerate() {
        let rrf = 1.0 / (K + (rank + 1) as f32);
        scores
            .entry(doc.id.clone())
            .and_modify(|(s, _)| *s += rrf)
            .or_insert((rrf, doc));
    }

    // BM25 hits — convert IndexDoc → MemoryDoc for docs not yet in map.
    for (rank, doc) in bm25_hits.into_iter().enumerate() {
        let rrf = 1.0 / (K + (rank + 1) as f32);
        scores
            .entry(doc.id.clone())
            .and_modify(|(s, _)| *s += rrf)
            .or_insert_with(|| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                (
                    rrf,
                    MemoryDoc {
                        id: doc.id,
                        scope: doc.scope,
                        kind: doc.kind,
                        text: doc.content,
                        vector: vec![],
                        created_at: now,
                        accessed_at: now,
                        access_count: 0,
                        importance: 0.5,
                        tier: Default::default(),
                        abstract_text: None,
                        overview_text: None,
                    },
                )
            });
    }

    // Apply lifecycle decay multiplier — older/less-accessed docs score lower.
    let mut ranked: Vec<(f32, MemoryDoc)> = scores
        .into_values()
        .map(|(score, doc)| (score * doc.decay_multiplier(), doc))
        .collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().take(top_k).map(|(_, doc)| doc).collect()
}

// ---------------------------------------------------------------------------
// Tool dispatch helpers — inject fields for backward-compat routing
// ---------------------------------------------------------------------------

/// Inject an `action` field into `args` if not already present.
fn inject_action(mut args: Value, action: &str) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.entry("action").or_insert_with(|| json!(action));
    }
    args
}

/// Inject a `channel` field into `args` if not already present.
fn inject_channel(mut args: Value, channel: &str) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.entry("channel").or_insert_with(|| json!(channel));
    }
    args
}

// ---------------------------------------------------------------------------
// Tool list builder
// ---------------------------------------------------------------------------

fn build_tool_list(
    skills: &SkillRegistry,
    agents: Option<&AgentRegistry>,
    caller_id: &str,
    external_agents: &[crate::config::schema::ExternalAgentConfig],
) -> Vec<ToolDef> {
    let mut tools = Vec::new();

    // Built-in tools — consolidated (32+ tools -> ~13 unified tools).
    tools.push(ToolDef {
        name: "memory".to_owned(),
        description: "Manage long-term memory. Actions: search (semantic search), get (by ID), put (store new), delete (remove by ID).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["search", "get", "put", "delete"], "description": "Action to perform"},
                "query":  {"type": "string", "description": "Search query (for search action)"},
                "id":     {"type": "string", "description": "Memory document ID (for get/delete)"},
                "text":   {"type": "string", "description": "Content to store (for put action)"},
                "scope":  {"type": "string", "description": "Scope filter (optional)"},
                "kind":   {"type": "string", "description": "Document kind: note, fact, summary, or remember. Use 'remember' ONLY when the user explicitly asks to remember/memorize something."},
                "top_k":  {"type": "integer", "description": "Max results (for search, default 5)"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "read_file".to_owned(),
        description: "Read a file from the agent workspace. Path is relative to workspace root."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative file path within the workspace"}
            },
            "required": ["path"]
        }),
    });
    tools.push(ToolDef {
        name: "write_file".to_owned(),
        description: "Write/create a file. Use this for ALL file creation and writing — do NOT use execute_command with notepad, echo, or any other editor/command to create files.\n\
            Creates parent directories as needed. Path is relative to workspace root.\n\
            Both 'path' and 'content' are required.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "Relative file path within the workspace (REQUIRED). Example: 'output.py'"},
                "content": {"type": "string", "description": "File content to write (REQUIRED)."}
            },
            "required": ["path", "content"]
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
        name: "execute_command".to_owned(),
        description: if cfg!(target_os = "windows") {
            "Run a shell command (PowerShell) on Windows.\n\
             IMPORTANT: For file listing use `list_dir`, for file search use `search_file`, for content search use `search_content`, for tool install use `install_tool`. Only use exec for commands that have no dedicated tool.\n\
             Use exec for: git operations, running scripts (node/python/cargo), system info (systeminfo, ipconfig, Get-Process), package management (npm/pip), process management (Start-Process, Stop-Process, taskkill).\n\
             PowerShell tips:\n\
             - Pipes: Get-Process | Sort-Object CPU -Descending | Select-Object -First 10\n\
             - Network: Test-NetConnection host -Port 80; Invoke-WebRequest -Uri <url>\n\
             - Text: (Get-Content file) -replace 'old','new'\n\
             - Dates: Get-Date -Format 'yyyy-MM-dd'; [DateTimeOffset]::Now.ToUnixTimeSeconds()\n\
             - Do NOT wrap commands in extra cmd /c or powershell -Command layers.\n\
             - Do NOT use exec for destructive operations on personal directories (Desktop, Downloads, Documents).\n\
             - For long-running processes (servers, watchers): use wait=false (default) to start without blocking.\n\
             - After starting a background task, poll for results using task_id parameter.\n\
             - If a command fails, do NOT retry with the same arguments. Try a different approach or ask the user."
                .to_owned()
        } else if cfg!(target_os = "macos") {
            "Run a shell command (bash/zsh) on macOS.\n\
             IMPORTANT: For file listing use `list_dir`, for file search use `search_file`, for content search use `search_content`, for tool install use `install_tool`. Only use exec for commands that have no dedicated tool.\n\
             Use exec for: git operations, running scripts (node/python/cargo), system info (uname, df, top), package management (brew/npm/pip), process management (ps, kill).\n\
             Tips: Use `date +%s` for Unix timestamps (never calculate manually). Use `| head -n 20` to limit output.\n\
             If a command fails, do NOT retry with the same arguments. Try a different approach or ask the user."
                .to_owned()
        } else {
            "Run a shell command (bash/sh) on Linux.\n\
             IMPORTANT: For file listing use `list_dir`, for file search use `search_file`, for content search use `search_content`, for tool install use `install_tool`. Only use exec for commands that have no dedicated tool.\n\
             Use exec for: git operations, running scripts (node/python/cargo), system info (uname, df, top), package management (apt/npm/pip), process management (ps, kill).\n\
             Tips: Use `date +%s` for Unix timestamps (never calculate manually). Use `| head -n 20` to limit output.\n\
             If a command fails, do NOT retry with the same arguments. Try a different approach or ask the user."
                .to_owned()
        },
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute. Must be valid for the current OS."},
                "wait": {"type": "boolean", "description": "If true, block until command completes and return result immediately. If false (default), run in background and return task_id. Use wait=true when you need the result to decide next steps."},
                "task_id": {"type": "string", "description": "Task ID to poll/check status (from a previous background exec). Returns result if completed, or 'running' status if still in progress."}
            },
            "required": []
        }),
    });
    tools.push(ToolDef {
        name: "agent".to_owned(),
        description: "Manage sub-agents. You are the architect — delegate work, never block.\n\
            Actions:\n\
            - task: Fire-and-forget one-shot task. Returns immediately with task_id. Result delivered on your next turn.\n\
            - spawn: Create a persistent sub-agent (survives across turns).\n\
            - send: Send a message to a spawned sub-agent (async, result on next turn).\n\
            - list: List all registered agents.\n\
            - kill: Stop a sub-agent.\n\
            Tips:\n\
            - Use task for independent, parallelizable work. You can dispatch multiple tasks at once.\n\
            - Always specify toolset matching the task (web for search, code for file ops).\n\
            - After dispatching, tell the user what you delegated and continue with other work.\n\
            - Check task results on your next response — they appear as [async task completed] messages.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["spawn", "task", "send", "list", "kill"], "description": "Action to perform"},
                "id":      {"type": "string", "description": "Agent ID (for spawn/send/kill)"},
                "model":   {"type": "string", "description": "Model string (for spawn/task)"},
                "system":  {"type": "string", "description": "Role description (for spawn/task)"},
                "message": {"type": "string", "description": "Message to send (for task/send)"},
                "toolset": {"type": "string", "enum": ["minimal", "standard", "web", "code", "full"], "description": "Tool access level. Default: standard."}
            },
            "required": ["action"]
        }),
    });

    // Tool installer (structured alternative to exec rsclaw tools install).
    tools.push(ToolDef {
        name: "install_tool".to_owned(),
        description: "Install a tool/runtime. Available: python, node, ffmpeg, chrome, opencode, claude-code, sherpa-onnx.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "enum": ["python", "node", "ffmpeg", "chrome", "opencode", "claude-code", "sherpa-onnx"], "description": "Tool name to install"}
            },
            "required": ["name"]
        }),
    });

    // File operation tools (structured alternatives to exec ls/find/grep).
    // These help small models avoid digit-loss and dead-loop issues.
    tools.push(ToolDef {
        name: "list_dir".to_owned(),
        description: "List files and directories in a given path. Use this instead of exec ls.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path":      {"type": "string", "description": "Directory path to list. Defaults to workspace root."},
                "recursive": {"type": "boolean", "description": "List recursively (default: false)"},
                "pattern":   {"type": "string", "description": "Glob pattern filter (e.g. '*.json', '*.rs')"}
            }
        }),
    });
    tools.push(ToolDef {
        name: "search_file".to_owned(),
        description: "Search for files by name pattern. Use this instead of exec find.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "File name pattern with wildcards (e.g. '*.log', 'config*')"},
                "path":    {"type": "string", "description": "Root directory to search in. Defaults to workspace."},
                "max_results": {"type": "integer", "description": "Maximum results to return (default: 20)"}
            },
            "required": ["pattern"]
        }),
    });
    tools.push(ToolDef {
        name: "search_content".to_owned(),
        description: "Search file contents by regex or text pattern. Use this instead of exec grep.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern":  {"type": "string", "description": "Text or regex pattern to search for"},
                "path":     {"type": "string", "description": "File or directory to search in. Defaults to workspace."},
                "include":  {"type": "string", "description": "File glob filter (e.g. '*.py', '*.rs')"},
                "ignore_case": {"type": "boolean", "description": "Case insensitive search (default: false)"},
                "max_results": {"type": "integer", "description": "Maximum results (default: 20)"}
            },
            "required": ["pattern"]
        }),
    });

    // Web tools.
    tools.push(ToolDef {
        name: "web_search".to_owned(),
        description: "Search the web for real-time information.\n\
            When to use:\n\
            - Questions beyond your knowledge cutoff or training data\n\
            - Current events, recent updates, time-sensitive information\n\
            - Latest documentation, API references, version-specific features\n\
            - When unsure about facts — search BEFORE saying 'I don't know'\n\
            Tips:\n\
            - Be specific: include version numbers, dates, or exact terms\n\
            - Use the current year (not past years) for latest docs\n\
            - For Chinese content, search in Chinese for better results".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query":    {"type": "string", "description": "Search query — be specific, include keywords and dates"},
                "provider": {"type": "string", "description": "Search provider: duckduckgo, google, bing, brave. Leave empty for default."},
                "limit":    {"type": "integer", "description": "Max results (default 5)"}
            },
            "required": ["query"]
        }),
    });
    tools.push(ToolDef {
        name: "web_fetch".to_owned(),
        description: "Fetch a web page and convert to readable text/markdown.\n\
            Use this to read documentation, articles, API docs, or any web content.\n\
            - URL must be fully-formed (https://...)\n\
            - HTTP auto-upgraded to HTTPS\n\
            - Falls back to browser rendering for JS-heavy pages\n\
            - Results cached 15 minutes\n\
            - For large pages, use 'prompt' to extract specific information\n\
            - This is read-only — does not modify anything\n\
            - If content is behind login, use web_browser instead".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "url":    {"type": "string", "description": "Full URL to fetch (e.g. https://docs.example.com/api)"},
                "prompt": {"type": "string", "description": "What to extract from the page (e.g. 'list all API endpoints')"}
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
            - Do NOT use execute_command with curl/wget — always use this tool\n\
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
        description: "Control a web browser. Core workflow:\n\
            1. `open` — navigate to a URL\n\
            2. `snapshot` — get page content with interactive element refs (@e1, @e2...)\n\
            3. `click` ref=@e1 / `fill` ref=@e2 text='...' — interact using refs from snapshot\n\
            4. Re-snapshot after any page change to get updated refs\n\
            Other actions: type, select, check, scroll, screenshot, pdf, press, back, forward, reload, wait, evaluate, cookies, get_text, get_url, get_title, find, get_article, upload, new_tab, switch_tab, close_tab.\n\
            IMPORTANT: Always snapshot BEFORE clicking/filling. Element refs change after page updates.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":     {"type": "string", "enum": [
                    "open", "navigate", "snapshot", "click", "fill", "type",
                    "select", "check", "uncheck", "scroll", "screenshot", "pdf",
                    "back", "forward", "reload", "get_text", "get_url", "get_title",
                    "wait", "evaluate", "cookies", "press", "set_viewport",
                    "dialog", "state", "network", "new_tab", "list_tabs",
                    "switch_tab", "close_tab", "highlight", "clipboard", "find",
                    "get_article", "upload", "context", "emulate", "diff", "record"
                ]},
                "url":        {"type": "string", "description": "URL for open/navigate"},
                "ref":        {"type": "string", "description": "Element ref like @e3 from snapshot"},
                "text":       {"type": "string", "description": "Text for fill/type/click-by-text/clipboard/dialog"},
                "value":      {"type": "string", "description": "Value for select, or sub-action for cookies/state/dialog/network/clipboard/context/emulate/diff/record"},
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
                "annotate":   {"type": "boolean", "description": "Overlay numbered labels on interactive elements"},
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
                "headed":     {"type": "boolean", "description": "true=foreground (visible window), false=background (headless). Default: auto-detect based on display availability. Omit this field to use the default."}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "computer_use".to_owned(),
        description: "Control the computer desktop. ONLY use when the user EXPLICITLY asks to take a screenshot, click, type, or interact with the desktop. Do NOT call this tool just because the message mentions words like 'screenshot' or 'screen' in other contexts. Screenshots auto-resize for HiDPI and return scale factor for coordinate mapping.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":    {"type": "string", "enum": [
                    "screenshot", "mouse_move", "mouse_click", "left_click",
                    "double_click", "triple_click", "right_click", "middle_click",
                    "drag", "scroll", "type", "key", "hold_key",
                    "cursor_position", "get_active_window", "wait"
                ], "description": "Action to perform"},
                "x":         {"type": "number", "description": "X coordinate (mouse actions, drag start)"},
                "y":         {"type": "number", "description": "Y coordinate (mouse actions, drag start)"},
                "to_x":      {"type": "number", "description": "Drag destination X"},
                "to_y":      {"type": "number", "description": "Drag destination Y"},
                "button":    {"type": "string", "enum": ["left", "right", "middle"], "description": "Mouse button (default: left)"},
                "text":      {"type": "string", "description": "Text for type action"},
                "key":       {"type": "string", "description": "Key name or combo (e.g. Enter, ctrl+c, cmd+shift+s)"},
                "then":      {"type": "string", "enum": ["click", "double_click", "right_click", "triple_click"], "description": "Sub-action for hold_key (default: click)"},
                "direction": {"type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction (default: down)"},
                "amount":    {"type": "integer", "description": "Scroll clicks (default: 3)"},
                "ms":        {"type": "integer", "description": "Wait duration in milliseconds (max 10000)"}
            },
            "required": ["action"]
        }),
    });

    // --- New openclaw-compatible tools ---

    tools.push(ToolDef {
        name: "image_gen".to_owned(),
        description: "Generate an image from a text description using an AI image model. Pass the user's original description as-is (preserve their language, do not translate).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Image description. IMPORTANT: use the user's original language and wording, do not translate to English."},
                "size":   {"type": "string", "description": "Image size, e.g. 2048x2048", "default": "2048x2048"}
            },
            "required": ["prompt"]
        }),
    });
    tools.push(ToolDef {
        name: "pdf".to_owned(),
        description: "Extract text content from a PDF file or URL.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path or URL to a PDF document"}
            },
            "required": ["path"]
        }),
    });
    tools.push(ToolDef {
        name: "text_to_voice".to_owned(),
        description: "Convert text to speech audio.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "text":  {"type": "string", "description": "Text to convert to speech"},
                "voice": {"type": "string", "description": "Voice name (macOS: say -v '?', Linux: espeak --voices)"}
            },
            "required": ["text"]
        }),
    });
    tools.push(ToolDef {
        name: "send_message".to_owned(),
        description: "Send a message to a chat channel target (user or group).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "channel": {"type": "string", "description": "Channel type (e.g. telegram, discord)"},
                "target":  {"type": "string", "description": "Target user or group ID"},
                "text":    {"type": "string", "description": "Message text to send"}
            },
            "required": ["target", "text"]
        }),
    });
    tools.push(ToolDef {
        name: "cron".to_owned(),
        description: "List, add, edit, remove, enable or disable cron jobs. For edit/remove/enable/disable, prefer using `index` from the list output instead of `id` to avoid ID truncation issues.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":   {"type": "string", "enum": ["list", "add", "edit", "remove", "enable", "disable"], "description": "Action to perform"},
                "schedule": {"type": "string", "description": "Cron schedule expression (for add, edit)"},
                "message":  {"type": "string", "description": "Message or task to run (for add, edit)"},
                "index":    {"type": "number", "description": "Job index from list (1-based, for edit/remove/enable/disable - preferred)"},
                "id":       {"type": "string", "description": "Job ID (for edit/remove/enable/disable - use index instead if possible)"},
                "name":     {"type": "string", "description": "Job name (for add, edit)"},
                "tz":       {"type": "string", "description": "Timezone e.g. Asia/Shanghai (for add, edit)"},
                "agentId":  {"type": "string", "description": "Agent ID to run the job (for add, edit, default: main)"}
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
        description: "Query gateway status and information.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["status", "health", "version"], "description": "Info to retrieve"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "opencode".to_owned(),
        description: "Execute coding tasks using OpenCode (a powerful coding agent). IMPORTANT: When creating new projects or files, ALWAYS create a dedicated project directory first (e.g., 'my-project/') and place all files inside it. Do NOT create files directly in the workspace root. The task will run asynchronously and results will be sent when complete.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The coding task to execute. Be specific about file paths and always mention creating a project subdirectory for new projects."}
            },
            "required": ["task"]
        }),
    });
    tools.push(ToolDef {
        name: "claudecode".to_owned(),
        description: "Execute coding tasks using Claude Code (official Claude Agent SDK via ACP protocol). Uses Claude's native coding capabilities with full context awareness. IMPORTANT: When creating new projects or files, ALWAYS create a dedicated project directory first. The task will run asynchronously and results will be sent when complete.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The coding task to execute. Be specific about requirements and file paths."}
            },
            "required": ["task"]
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

    // Document creation & editing tool.
    tools.push(ToolDef {
        name: "doc".to_owned(),
        description: "Create, edit, and read documents. Use this for ALL document operations — do NOT use execute_command.\n\
            Supported formats: xlsx, xls, docx, doc, pdf, pptx, ppt, txt, md, csv\n\
            Actions:\n\
            - read_doc: Read any document (xlsx/docx/pdf/pptx/txt/md/csv). Returns text content.\n\
            - create_excel: Create xlsx with sheets [{name, headers, rows}]\n\
            - create_word: Create docx with content (# for headings, blank lines for paragraphs)\n\
            - create_pdf: Create PDF with content\n\
            - create_ppt: Create pptx with slides [{title, body}]\n\
            - edit_excel: Update sheets or append_rows to existing xlsx\n\
            - edit_word: Replace content or append text to existing docx\n\
            - edit_pdf: replace_text [{find,replace}], delete_pages [1,3]\n\
            Tips:\n\
            - For txt/md: use read_file/write_file instead (simpler)\n\
            - For csv: use read_doc to read, create_excel to convert to xlsx\n\
            - To edit PPT: read_doc first, then create_ppt with modified slides\n\
            - After creating, use send_file to deliver to the user".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":  {"type": "string", "enum": ["create_excel", "create_word", "create_pdf", "create_ppt", "edit_excel", "edit_word", "edit_pdf", "read_doc"], "description": "Action to perform"},
                "path":    {"type": "string", "description": "File path relative to workspace, e.g. 'report.xlsx'"},
                "title":   {"type": "string", "description": "Document title (optional, for word/pdf)"},
                "sheets":  {"type": "array", "description": "For create_excel/edit_excel: [{name, headers: [str], rows: [[value]]}]. For edit_excel, matching sheet names replace existing sheets; new names add sheets.",
                    "items": {"type": "object", "properties": {
                        "name":    {"type": "string"},
                        "headers": {"type": "array", "items": {"type": "string"}},
                        "rows":    {"type": "array", "items": {"type": "array"}}
                    }}
                },
                "append_rows": {"description": "For edit_excel: append rows to existing sheet. Either {sheet, rows: [[value]]} or [{sheet, rows}]"},
                "content": {"type": "string", "description": "For create_word/create_pdf: text content. For edit_word: replacement content (replaces entire document). Paragraphs separated by blank lines. Lines starting with # are headings."},
                "append":  {"type": "string", "description": "For edit_word: text to append to existing document. Paragraphs separated by blank lines. Lines starting with # are headings."},
                "replacements": {"type": "array", "description": "For edit_pdf replace_text: [{find: 'old', replace: 'new'}]. Works on raw PDF content streams — may not work for text split across operators.",
                    "items": {"type": "object", "properties": {
                        "find":    {"type": "string"},
                        "replace": {"type": "string"}
                    }}
                },
                "delete_pages": {"type": "array", "description": "For edit_pdf: 1-indexed page numbers to delete, e.g. [1, 3]",
                    "items": {"type": "integer"}
                },
                "slides":  {"type": "array", "description": "For create_ppt: [{title, body}]",
                    "items": {"type": "object", "properties": {
                        "title": {"type": "string"},
                        "body":  {"type": "string"}
                    }}
                }
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
                description: format!(
                    "Send a task to agent '{}'. Returns the agent's reply.",
                    handle.id
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
    }

    // External remote agent A2A tools (remote gateways).
    tracing::debug!(
        count = external_agents.len(),
        "build_tool_list: external agents"
    );
    for ext in external_agents {
        if ext.id == caller_id {
            continue;
        }
        tools.push(ToolDef {
            name: format!("agent_{}", ext.id),
            description: format!(
                "Send a task to remote agent '{}' at {}. Returns the agent's reply.",
                ext.id, ext.url
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

    tools
}

// ---------------------------------------------------------------------------
// Context pruning
// ---------------------------------------------------------------------------

/// Prune the session message history in-place according to config.
///
/// Strategy (applied in order):
///   1. Hard-clear: if total chars > threshold, keep only the last user
///      message.
///   2. Soft-trim: if total chars > tail_chars limit, remove old Tool messages
///      (oldest first, only if >= min_prunable_tool_chars).
/// Strip image data URIs from all but the last user message to prevent
/// context bloat. Replaces `ContentPart::Image` with `ContentPart::Text`
/// placeholder in older messages.
/// Check write safety:
/// 1. Block absolute paths (must stay within workspace)
/// 2. Block path traversal (../)
/// 3. Block sensitive filenames
/// 4. Scan ALL file content for dangerous commands (not just scripts)
/// Compress an image for LLM: resize to max 1024px and convert to JPEG.
/// Uses the `image` crate (pure Rust, cross-platform — no ffmpeg/sips needed).
/// Returns data URI or None if compression fails.
fn compress_image_for_llm(data_uri: &str) -> Option<String> {
    let b64 = data_uri
        .strip_prefix("data:image/png;base64,")
        .or_else(|| data_uri.strip_prefix("data:image/jpeg;base64,"))
        .or_else(|| data_uri.strip_prefix("data:image/webp;base64,"))
        .or_else(|| data_uri.strip_prefix("data:image/gif;base64,"))
        .unwrap_or(data_uri);

    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;

    // Skip if already small enough (<200KB)
    if bytes.len() < 200_000 {
        return Some(data_uri.to_owned());
    }

    let img = image::load_from_memory(&bytes).ok()?;

    // Resize so neither dimension exceeds 1024px, preserving aspect ratio.
    const MAX_DIM: u32 = 1024;
    let (w, h) = (img.width(), img.height());
    let img = if w > MAX_DIM || h > MAX_DIM {
        img.resize(MAX_DIM, MAX_DIM, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // Encode to JPEG quality 85.
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Jpeg).ok()?;
    let compressed = buf.into_inner();

    let b64 = base64::engine::general_purpose::STANDARD.encode(&compressed);
    tracing::debug!(
        original = bytes.len(),
        compressed = compressed.len(),
        "image compressed for LLM"
    );
    Some(format!("data:image/jpeg;base64,{b64}"))
}

/// Maximum characters to send from file content to LLM.
#[allow(dead_code)]
const MAX_FILE_CONTENT_CHARS: usize = 20_000;

fn check_write_safety(path: &str, full: &std::path::Path, content: &str) -> anyhow::Result<()> {
    // 1. Block absolute paths — write must be relative to workspace
    if path.starts_with('/') || path.starts_with('\\') || path.contains(":\\") {
        anyhow::bail!(
            "[blocked] absolute path not allowed: {path}. Use relative paths within workspace."
        );
    }

    // 2. Block path traversal
    if path.contains("../") || path.contains("..\\") {
        anyhow::bail!("[blocked] path traversal not allowed: {path}");
    }

    // 3. Block sensitive filenames (even within workspace)
    let path_lower = path.to_lowercase();
    const SENSITIVE_NAMES: &[&str] = &[
        ".bashrc",
        ".bash_profile",
        ".zshrc",
        ".profile",
        ".login",
        "authorized_keys",
        "known_hosts",
        "id_rsa",
        "id_ed25519",
        "crontab",
        ".env",
        "openclaw.json",
        "rsclaw.json5",
        "auth-profiles.json",
    ];
    let filename = full
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    for sensitive in SENSITIVE_NAMES {
        if filename == *sensitive || path_lower.ends_with(sensitive) {
            anyhow::bail!("[blocked] write to sensitive file: {path}");
        }
    }

    // 4. Scan ALL file content for dangerous commands
    if !content.is_empty() {
        let preparse = crate::agent::preparse::PreParseEngine::load();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('#')
                || trimmed.starts_with("//")
                || trimmed.starts_with("--")
            {
                continue;
            }
            match preparse.check_exec_safety(trimmed) {
                crate::agent::preparse::SafetyCheck::Deny(reason) => {
                    anyhow::bail!("[blocked] file contains dangerous command: {reason}");
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Check read safety: block access to sensitive files and directories.
fn check_read_safety(path: &str, full: &std::path::Path) -> anyhow::Result<()> {
    let path_str = full.to_string_lossy().to_lowercase();
    let path_lower = path.to_lowercase();

    // Sensitive directories
    const SENSITIVE_DIRS: &[&str] = &[
        ".ssh/",
        ".gnupg/",
        ".gpg/",
        ".aws/",
        ".azure/",
        ".gcloud/",
        ".config/gcloud/",
        ".kube/",
        ".docker/",
        ".claude/",
        ".opencode/",
        ".openclaw/credentials/",
        ".rsclaw/credentials/",
    ];
    for dir in SENSITIVE_DIRS {
        if path_lower.contains(dir) || path_str.contains(dir) {
            anyhow::bail!("[blocked] access to sensitive directory: {path}");
        }
    }

    // Sensitive filenames (private keys, credentials, tokens, etc.)
    let filename = full
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    const SENSITIVE_FILES: &[&str] = &[
        // SSH keys
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        "id_dsa",
        "id_rsa.pub",
        "id_ed25519.pub",
        "authorized_keys",
        "known_hosts",
        // GPG
        "secring.gpg",
        "trustdb.gpg",
        // Cloud credentials
        "credentials",
        "credentials.json",
        "credentials.yaml",
        "service_account.json",
        "application_default_credentials.json",
        // Env / secrets
        ".env",
        ".env.local",
        ".env.production",
        ".env.secret",
        ".netrc",
        ".npmrc",
        ".pypirc",
        // Shell config (may contain tokens/aliases)
        ".bash_history",
        ".zsh_history",
        // Database
        ".pgpass",
        ".my.cnf",
        ".mongoshrc.js",
        // Docker / Kube
        "config.json", // docker config with auth
        // Crypto wallets
        "wallet.dat",
        "keystore",
        // AI tool config files (contain API keys)
        "openclaw.json",
        "rsclaw.json5",
        "auth-profiles.json",
    ];

    for sensitive in SENSITIVE_FILES {
        if filename == *sensitive {
            anyhow::bail!("[blocked] access to sensitive file: {path}");
        }
    }

    // Private key content pattern in filename
    if filename.contains("private") && (filename.contains("key") || filename.ends_with(".pem")) {
        anyhow::bail!("[blocked] access to private key file: {path}");
    }

    // Block reading system auth files via absolute path
    const SYSTEM_FILES: &[&str] = &[
        "/etc/shadow",
        "/etc/gshadow",
        "/etc/master.passwd",
        "/etc/sudoers",
    ];
    for sys in SYSTEM_FILES {
        if path_str.ends_with(sys) || path == *sys {
            anyhow::bail!("[blocked] access to system file: {path}");
        }
    }

    Ok(())
}

/// Scan a file's content against exec deny rules.
/// Used when an interpreter (bash, python, etc.) executes a file.
fn check_file_content_safety(file_path: &std::path::Path) -> anyhow::Result<()> {
    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return Ok(()), // file doesn't exist or not readable, let exec handle it
    };
    let preparse = crate::agent::preparse::PreParseEngine::load();
    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || trimmed.starts_with("--")
        {
            continue;
        }
        match preparse.check_exec_safety(trimmed) {
            crate::agent::preparse::SafetyCheck::Deny(reason) => {
                anyhow::bail!(
                    "[blocked] file {}:{} contains dangerous command: {reason}",
                    file_path.display(),
                    line_num + 1
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// Estimate token count for mixed-language text.
/// - ASCII/Latin: ~4 chars per token
/// - CJK (Chinese/Japanese/Korean): ~1.5 chars per token
/// - Other Unicode: ~2 chars per token
pub fn estimate_tokens(text: &str) -> usize {
    let mut ascii_chars = 0usize;
    let mut cjk_chars = 0usize;
    let mut other_chars = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3400}'..='\u{4DBF}').contains(&ch)
            || ('\u{3000}'..='\u{303F}').contains(&ch)
            || ('\u{FF00}'..='\u{FFEF}').contains(&ch)
            || ('\u{AC00}'..='\u{D7AF}').contains(&ch)
        {
            cjk_chars += 1;
        } else {
            other_chars += 1;
        }
    }
    (ascii_chars / 4) + (cjk_chars * 3 / 2) + (other_chars / 2)
}

fn strip_old_images(mut messages: Vec<Message>) -> Vec<Message> {
    // Find the index of the last user message (the one that may have fresh images).
    let last_user_idx = messages.iter().rposition(|m| m.role == Role::User);

    for (i, msg) in messages.iter_mut().enumerate() {
        if Some(i) == last_user_idx {
            continue; // keep images on the latest user message
        }
        if let MessageContent::Parts(parts) = &msg.content {
            let has_image = parts.iter().any(|p| matches!(p, ContentPart::Image { .. }));
            if has_image {
                // Replace with text-only version
                let text: String = parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                msg.content = MessageContent::Text(if text.is_empty() {
                    "[image]".to_owned()
                } else {
                    format!("{text} [image]")
                });
            }
        }
    }
    // Note: Tool-role messages are kept in the request to allow multi-turn
    // tool calling. They are stripped from persisted session history instead
    // (see append_to_session) to avoid ordering violations on next turn.
    messages
}

fn apply_context_pruning(messages: &mut Vec<Message>, cfg: Option<&ContextPruningConfig>) {
    let Some(cfg) = cfg else { return };

    let total: usize = messages.iter().map(msg_chars).sum();

    // Hard clear.
    if let Some(hc) = &cfg.hard_clear
        && hc.enabled.unwrap_or(false)
    {
        let threshold = hc.threshold.unwrap_or(200_000) as usize;
        if total > threshold {
            let last_user = messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .cloned();
            messages.clear();
            if let Some(m) = last_user {
                messages.push(m);
            }
            return;
        }
    }

    // Soft trim.
    if let Some(st) = &cfg.soft_trim
        && st.enabled.unwrap_or(false)
    {
        let limit = st.tail_chars.unwrap_or(80_000) as usize;
        let min_prunable = cfg.min_prunable_tool_chars.unwrap_or(500) as usize;

        if total > limit {
            let mut chars_over = total - limit;
            let mut to_remove: Vec<usize> = Vec::new();
            for (i, msg) in messages.iter().enumerate() {
                if chars_over == 0 {
                    break;
                }
                if msg.role == Role::Tool {
                    let c = msg_chars(msg);
                    if c >= min_prunable {
                        to_remove.push(i);
                        chars_over = chars_over.saturating_sub(c);
                    }
                }
            }
            for i in to_remove.into_iter().rev() {
                messages.remove(i);
            }
        }
    }
}

fn msg_chars(m: &Message) -> usize {
    match &m.content {
        MessageContent::Text(t) => t.len(),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => text.len(),
                _ => 50,
            })
            .sum(),
    }
}

/// Build a summary Message from the last 10 user/assistant messages (for /clear).
fn build_clear_summary(messages: &[Message]) -> Option<Message> {
    if messages.is_empty() { return None; }
    let recent: Vec<&Message> = messages.iter().rev().take(10).rev().collect();
    let mut parts = Vec::new();
    for m in &recent {
        let role = match m.role {
            crate::provider::Role::User => "User",
            crate::provider::Role::Assistant => "Assistant",
            _ => continue,
        };
        let text = match &m.content {
            crate::provider::MessageContent::Text(s) => s.clone(),
            crate::provider::MessageContent::Parts(ps) => ps.iter().filter_map(|p| {
                if let crate::provider::ContentPart::Text { text } = p { Some(text.as_str()) } else { None }
            }).collect::<Vec<_>>().join(" "),
        };
        if text.is_empty() { continue; }
        let truncated = if text.chars().count() > 200 {
            let idx = text.char_indices().nth(200).map(|(i, _)| i).unwrap_or(text.len());
            format!("{}...", &text[..idx])
        } else { text };
        parts.push(format!("{role}: {truncated}"));
    }
    if parts.is_empty() { return None; }
    Some(Message {
        role: crate::provider::Role::System,
        content: crate::provider::MessageContent::Text(
            format!("[Session summary before /clear]\n{}", parts.join("\n"))
        ),
    })
}

/// CJK-aware token estimate for a message (used by compaction threshold).
fn msg_tokens(m: &Message) -> usize {
    let text = match &m.content {
        MessageContent::Text(t) => t.as_str(),
        MessageContent::Parts(parts) => {
            // Sum tokens from each text part
            return parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => estimate_tokens(text),
                    _ => 50,
                })
                .sum();
        }
    };
    estimate_tokens(text)
}

/// Trim session messages from oldest to newest so the total history fits
/// within the model's context budget. Uses `chars / 4` as a token estimate.
///
/// Budget calculation:
///   reply_reserve  = max(context_budget * 20%, 2000)
///   system_tokens  = system_prompt.len() / 4
///   tools_tokens   = tools JSON size / 4
///   history_budget = context_budget - reply_reserve - system_tokens -
/// tools_tokens
///
/// Always keeps at least the last 3 user-assistant pairs (6 messages).
fn apply_context_budget_trim(
    messages: &mut Vec<Message>,
    context_tokens: usize,
    system_prompt: &str,
    tools: &[ToolDef],
) {
    if messages.len() <= 6 {
        return;
    }

    let reply_reserve = (context_tokens / 5).max(2000);
    let sys_tokens = estimate_tokens(system_prompt);
    // Estimate tool definitions size from JSON serialization.
    let tools_tokens = serde_json::to_string(tools)
        .map(|s| estimate_tokens(&s))
        .unwrap_or(0);

    let history_budget = context_tokens
        .saturating_sub(reply_reserve)
        .saturating_sub(sys_tokens)
        .saturating_sub(tools_tokens);

    let total_tokens: usize = messages.iter().map(msg_tokens).sum();
    if total_tokens <= history_budget {
        return;
    }

    // Trim from the front, keeping at least the last 6 messages.
    let min_keep = 6;
    let max_removable = messages.len().saturating_sub(min_keep);
    let mut removed_tokens: usize = 0;

    let mut remove_count = 0;
    for i in 0..max_removable {
        if total_tokens - removed_tokens <= history_budget {
            break;
        }
        removed_tokens += msg_tokens(&messages[i]);
        remove_count += 1;
    }

    if remove_count > 0 {
        tracing::info!(
            context_tokens,
            history_budget,
            total_tokens,
            removed = remove_count,
            remaining = messages.len() - remove_count,
            "context budget trim: removed {remove_count} oldest messages"
        );
        messages.drain(..remove_count);

        // Insert a system-like marker so the model knows history was truncated.
        // This prevents the model from repeating itself due to missing context.
        messages.insert(0, Message {
            role: Role::User,
            content: MessageContent::Text(
                "[System: earlier conversation history was trimmed to fit context window. Continue naturally from the messages below.]".to_owned()
            ),
        });
        messages.insert(1, Message {
            role: Role::Assistant,
            content: MessageContent::Text("Understood.".to_owned()),
        });
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Persist dynamic agent to config file
// ---------------------------------------------------------------------------

/// Append an AgentEntry to the `agents.list` array in the config file.
/// The hot-reload watcher will pick up the change automatically.
async fn persist_agent_to_config(entry: &crate::config::schema::AgentEntry) -> anyhow::Result<()> {
    let config_path = crate::config::loader::detect_config_path()
        .ok_or_else(|| anyhow!("no config file found"))?;
    let raw = tokio::fs::read_to_string(&config_path).await?;
    let mut doc: serde_json::Value = json5::from_str(&raw)
        .map_err(|e| anyhow!("parse config: {e}"))?;

    // Don't duplicate if agent already exists.
    let id = entry.id.as_str();
    let already_exists = doc.pointer("/agents/list")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().any(|e| e.get("id").and_then(|v| v.as_str()) == Some(id)))
        .unwrap_or(false);
    if already_exists {
        return Ok(());
    }

    let mut entry_val = serde_json::to_value(entry)?;

    // Strip model field if it matches agents.defaults.model.primary
    // (no need to persist what the defaults already provide).
    let defaults_primary = doc.pointer("/agents/defaults/model/primary")
        .and_then(|v| v.as_str()).map(|s| s.to_owned());
    let entry_primary = entry_val.pointer("/model/primary")
        .and_then(|v| v.as_str()).map(|s| s.to_owned());
    if defaults_primary.is_some() && defaults_primary == entry_primary {
        entry_val.as_object_mut().map(|o| o.remove("model"));
    }

    let list = doc
        .pointer_mut("/agents/list")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("agents.list not found in config"))?;
    list.push(entry_val);

    // Write back as pretty JSON (json5-compatible).
    let output = serde_json::to_string_pretty(&doc)?;
    tokio::fs::write(&config_path, output).await?;
    tracing::info!(agent_id = %id, "agent persisted to config");
    Ok(())
}

// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::schema::{ContextPruningConfig, HardClearConfig, SoftTrimConfig},
        provider::{Message, MessageContent, Role},
        skill::SkillRegistry,
    };

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_owned()),
        }
    }

    // ------------------------------------------------------------------
    // msg_chars
    // ------------------------------------------------------------------

    #[test]
    fn msg_chars_text_variant() {
        let m = text_msg(Role::User, "hello");
        assert_eq!(msg_chars(&m), 5);
    }

    #[test]
    fn msg_chars_parts_variant() {
        let m = Message {
            role: Role::Assistant,
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "abc".to_owned(),
                },
                ContentPart::Text {
                    text: "de".to_owned(),
                },
            ]),
        };
        assert_eq!(msg_chars(&m), 5);
    }

    // ------------------------------------------------------------------
    // apply_context_pruning — hard clear
    // ------------------------------------------------------------------

    #[test]
    fn hard_clear_removes_all_but_last_user() -> anyhow::Result<()> {
        let mut msgs = vec![
            text_msg(Role::User, &"u".repeat(50_000)),
            text_msg(Role::Assistant, &"a".repeat(50_000)),
            text_msg(Role::Tool, &"t".repeat(50_000)),
            text_msg(Role::User, "last user message"),
        ];

        let cfg = ContextPruningConfig {
            mode: None,
            ttl: None,
            keep_last_assistants: None,
            min_prunable_tool_chars: None,
            soft_trim: None,
            hard_clear: Some(HardClearConfig {
                enabled: Some(true),
                threshold: Some(100_000),
            }),
            tools: None,
        };

        apply_context_pruning(&mut msgs, Some(&cfg));

        assert_eq!(msgs.len(), 1, "hard clear should leave only one message");
        assert_eq!(msgs[0].role, Role::User);
        match &msgs[0].content {
            MessageContent::Text(t) => assert_eq!(t, "last user message"),
            other => return Err(anyhow::anyhow!("expected Text content, got {:?}", other)),
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // apply_context_pruning — soft trim removes large Tool messages
    // ------------------------------------------------------------------

    #[test]
    fn soft_trim_removes_large_tool_messages() {
        let large_tool = "x".repeat(2_000);
        let mut msgs = vec![
            text_msg(Role::User, "hi"),
            text_msg(Role::Tool, &large_tool),
            text_msg(Role::Assistant, "response"),
        ];

        let cfg = ContextPruningConfig {
            mode: None,
            ttl: None,
            keep_last_assistants: None,
            min_prunable_tool_chars: Some(500),
            soft_trim: Some(SoftTrimConfig {
                enabled: Some(true),
                head_chars: None,
                tail_chars: Some(500), // well below total so trim fires
            }),
            hard_clear: None,
            tools: None,
        };

        apply_context_pruning(&mut msgs, Some(&cfg));

        // The large Tool message should have been removed.
        let has_tool = msgs.iter().any(|m| m.role == Role::Tool);
        assert!(!has_tool, "large Tool message should have been pruned");
    }

    // ------------------------------------------------------------------
    // build_tool_list always contains the built-in tools
    // ------------------------------------------------------------------

    #[test]
    fn build_tool_list_contains_builtins() {
        let skills = SkillRegistry::new();
        let tools = build_tool_list(&skills, None, "test-agent", &[]);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        for expected in &[
            "memory", "session", "agent", "channel", "read_file", "write_file", "execute_command",
        ] {
            assert!(
                names.contains(expected),
                "expected built-in tool `{expected}` in tool list, got: {names:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// On-demand skill matching
// ---------------------------------------------------------------------------

/// Match user text against installed skills by keyword overlap.
/// Returns skills whose description or name keywords appear in the user text.
/// Only returns prompt-only skills (no tools) since tool-based skills are
/// already available via the tool list.
fn match_skills<'a>(
    text: &str,
    skills: &'a crate::skill::SkillRegistry,
) -> Vec<&'a crate::skill::SkillManifest> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let lower = text.to_lowercase();
    let mut matched = Vec::new();

    for skill in skills.all() {
        // Skip tool-based skills — they're already in the tool list.
        if !skill.tools.is_empty() {
            continue;
        }
        // Skip skills with no prompt body.
        if skill.prompt.trim().is_empty() {
            continue;
        }

        // Build keyword set from skill name + description.
        let mut keywords: Vec<&str> = Vec::new();

        // Name-derived keywords (split on hyphens/spaces).
        for part in skill.name.split(|c: char| c == '-' || c == '_' || c == ' ') {
            let p = part.trim();
            if p.len() >= 2 {
                keywords.push(p);
            }
        }

        // Description keywords (Chinese + English).
        if let Some(ref desc) = skill.description {
            // Extract meaningful words/phrases from description.
            for word in desc.split(|c: char| !c.is_alphanumeric() && c != '/' && c != '.') {
                let w = word.trim();
                if w.len() >= 2 {
                    keywords.push(w);
                }
            }
        }

        // Check if any keyword matches the user text.
        let hit = keywords.iter().any(|kw| {
            let kl = kw.to_lowercase();
            // Skip very generic words.
            if matches!(kl.as_str(), "the" | "and" | "for" | "with" | "use" | "when" | "from"
                | "create" | "edit" | "file" | "files" | "data" | "tool" | "agent"
                | "的" | "和" | "在" | "是" | "了" | "等") {
                return false;
            }
            lower.contains(&kl)
        });

        if hit {
            matched.push(skill);
        }
    }

    matched
}

/// Create a `tokio::process::Command` for PowerShell that hides the console window.
/// On Windows, sets CREATE_NO_WINDOW and -WindowStyle Hidden so the user never
/// sees a PowerShell flash. On other platforms, returns a plain command (for cross-compile).
fn powershell_hidden() -> tokio::process::Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = tokio::process::Command::new("powershell");
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        cmd.arg("-NoProfile").arg("-WindowStyle").arg("Hidden");
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut cmd = tokio::process::Command::new("powershell");
        cmd.arg("-NoProfile");
        cmd
    }
}
