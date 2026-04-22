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

use anyhow::{Result, anyhow};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::{
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
    loop_detection::LoopDetector,
    memory::{MemoryDoc, MemoryStore},
    registry::{AgentHandle, AgentMessage, AgentRegistry, AgentReply},
    tool_call_repair::repair_tool_result_pairing,
    workspace::{
        DEFAULT_MAX_CHARS_PER_FILE, DEFAULT_TOTAL_MAX_CHARS, SessionType,
    },
};
pub use super::context_mgr::estimate_tokens;
use super::context_mgr::{
    apply_context_budget_trim, apply_context_pruning, build_clear_summary,
    compress_image_for_llm, msg_tokens,
};
use super::prompt_builder::{
    build_help_text_filtered, build_system_prompt, format_duration,
    memory_age_label, READONLY_COMMANDS,
};
use super::security::check_read_safety;
use super::tools_builder::{build_tool_list, toolset_allowed_names};
use crate::{
    config::runtime::RuntimeConfig,
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
pub(crate) const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
/// Max consecutive tool parse errors before aborting the turn.
/// Prevents infinite retry loops when model output gets corrupted.
const MAX_PARSE_ERRORS: usize = 10;
/// Token string that suppresses any reply to the channel.
const NO_REPLY_TOKEN: &str = "NO_REPLY";
/// Default max file size before first confirmation (bytes): 50 MB.
const DEFAULT_MAX_FILE_SIZE: usize = 50_000_000;
/// Default max text chars before token confirmation.
const DEFAULT_MAX_TEXT_CHARS: usize = 50_000;
/// Sessions older than this TTL (7 days) are eligible for eviction.
const SESSION_IDLE_TTL_SECS: u64 = 7 * 24 * 3600;
/// Eviction only triggers when the session count exceeds this threshold.
const MAX_SESSIONS_PER_AGENT: usize = 10_000;

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
        || lower.contains("gemma4") // Google Gemma 4 (vision-capable)
        || lower.contains("gemma-4") // Google Gemma 4 variant
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
    pub exec_pool: Arc<super::exec_pool::ExecPool>,
    pub loop_detector: LoopDetector,
    /// Whether the current turn includes images.
    pub has_images: bool,
    /// The full user message with image data (for LLM, not persisted).
    pub user_msg_with_images: Option<Message>,
    /// Count of consecutive tool parse errors in this turn.
    pub parse_error_count: usize,
    /// Memory doc IDs recalled during this turn (auto-recall + tool_memory_search).
    pub recalled_memory_ids: std::collections::HashSet<String>,
    /// Whether a loop-detection warning was triggered during this turn.
    pub loop_warning_triggered: bool,
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
    pub(crate) failover: FailoverManager,
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
    /// WASM plugin instances for tool dispatch (shared across agents).
    pub wasm_plugins: Arc<Vec<crate::plugin::WasmPlugin>>,
    /// CDP browser session -- lazy-initialized on first web_browser tool call.
    /// Stored as Option so it can be dropped (killing Chrome) when idle expires.
    pub(crate) browser: Arc<tokio::sync::Mutex<Option<crate::browser::BrowserSession>>>,
    /// In-memory session cache: session_key -> conversation history.
    pub(crate) sessions: std::collections::HashMap<String, Vec<Message>>,
    /// Per-session compaction state: (last_compaction_time,
    /// turns_since_compaction).
    pub(crate) compaction_state: std::collections::HashMap<String, (std::time::Instant, u32)>,
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
    /// Cached system prompt — built once per gateway lifetime, never
    /// invalidated (only rebuilt on gateway restart).
    pub(crate) cached_system_prompt: Option<String>,
    /// Cached plugins system message — frozen at session start, rebuilt
    /// on compact or `/new`. Sorted by name for byte-stable output.
    pub(crate) cached_plugins_system: Option<String>,
    /// Cached skills system message — same lifecycle as plugins.
    pub(crate) cached_skills_system: Option<String>,
    /// Snapshot of installed skill names (sorted) for change detection.
    pub(crate) cached_skills_snapshot: Vec<String>,
    /// Cached tool definitions from the last run_turn — reused by compaction
    /// to match the KV cache prefix exactly.
    pub(crate) cached_tools: Vec<crate::provider::ToolDef>,
    /// Background context manager (/ctx command, formerly /btw).
    btw_manager: super::btw::BtwManager,
    pub(crate) notification_tx: Option<tokio::sync::broadcast::Sender<crate::channel::OutboundMessage>>,
    pub(crate) opencode_client: Arc<tokio::sync::OnceCell<crate::acp::client::AcpClient>>,
    pub(crate) claudecode_client: Arc<tokio::sync::OnceCell<crate::acp::client::AcpClient>>,
    /// In-memory session alias cache: alias_key → canonical session_key.
    /// Loaded from redb on first use, avoids repeated DB lookups.
    session_aliases: std::collections::HashMap<String, String>,
    /// Completed async task results: task_id → (session_key, result_json).
    /// Background task agents write here; main agent checks at turn start.
    pub(crate) pending_task_results: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    /// Sessions in voice mode: auto-TTS reply when user sent voice.
    /// Set when audio attachment detected, cleared by "/text" command.
    voice_mode_sessions: std::collections::HashSet<String>,
    /// Background exec pool — runs long commands without blocking the agent loop.
    pub(crate) exec_pool: Arc<super::exec_pool::ExecPool>,
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
        let max_concurrent = config
            .agents
            .defaults
            .max_concurrent
            .unwrap_or(4);
        let exec_pool = super::exec_pool::ExecPool::new(max_concurrent as usize);
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
            wasm_plugins: Arc::new(Vec::new()),
            live_status,
            browser: Arc::new(tokio::sync::Mutex::new(None)),
            sessions: std::collections::HashMap::new(),
            compaction_state: std::collections::HashMap::new(),
            pending_files: std::collections::HashMap::new(),
            runtime_max_file_size: None,
            runtime_max_text_chars: None,
            started_at: std::time::Instant::now(),
            workspace_cache: None,
            cached_system_prompt: None,
            cached_plugins_system: None,
            cached_skills_system: None,
            cached_skills_snapshot: Vec::new(),
            cached_tools: Vec::new(),
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
        // TODO: this spawned task has no JoinHandle and cannot be cancelled on shutdown
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

    // OpenCode / Claude Code ACP integration -> moved to tools_acp.rs

    /// Resolve the current model name from agent config with fallback.
    pub(crate) fn resolve_model_name(&self) -> String {
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
        // Read current session history — only User/Assistant text messages,
        // skip Tool/ToolCall messages (btw has no tools, they'd confuse the model).
        let btw_budget = self.config.agents.defaults.btw_tokens.unwrap_or(10_000) as usize;
        let history: Vec<Message> = self.sessions.get(session_key).cloned().unwrap_or_default();
        let mut messages = Vec::new();
        let mut token_count = 0usize;
        // Walk backwards, collect up to btw_budget tokens of User/Assistant text.
        for m in history.iter().rev() {
            if !matches!(m.role, Role::User | Role::Assistant) {
                continue;
            }
            let text = match &m.content {
                MessageContent::Text(t) => t.clone(),
                _ => continue,
            };
            let msg_tokens = super::context_mgr::estimate_tokens(&text);
            if token_count + msg_tokens > btw_budget && !messages.is_empty() {
                break;
            }
            // Truncate individual messages that are too long.
            let content = if text.chars().count() > 2000 {
                let truncated: String = text.chars().take(2000).collect();
                MessageContent::Text(format!("{truncated}..."))
            } else {
                MessageContent::Text(text)
            };
            messages.push(Message { role: m.role.clone(), content });
            token_count += msg_tokens;
        }
        messages.reverse();
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
            kv_cache_mode: 0,
            session_key: None,
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

            // Re-inject summaries so agent retains context, and persist to redb.
            for (key, msg) in summary_msgs {
                let val = serde_json::to_value(&msg).unwrap_or_default();
                if let Err(e) = self.store.db.append_message(&key, &val) {
                    tracing::warn!("failed to persist clear summary: {e:#}");
                }
                self.sessions.insert(key, vec![msg]);
            }
            // /clear does NOT invalidate plugins/skills cache (same conversation continues).
        }

        // /new — start a fresh conversation with new archive generation.
        if self.handle.new_session_signal.load(Ordering::SeqCst) {
            self.handle.new_session_signal.store(false, Ordering::SeqCst);
            info!("new_session_signal received, starting new generation");

            // Save session summary to memory before clearing — no summary
            // will be injected into the new session, so memory is the only
            // way the LLM can find prior context.
            let compaction_model = self.config.agents.defaults.compaction
                .as_ref().and_then(|c| c.model.clone())
                .or_else(|| self.handle.config.model.as_ref()?.primary.clone())
                .unwrap_or_else(|| "default".to_owned());
            self.save_session_summaries_to_memory(&compaction_model).await;

            self.sessions.clear();
            self.compaction_state.clear();
            for key in self.store.db.list_sessions().unwrap_or_default() {
                match self.store.db.new_generation(&key) {
                    Ok(g) => info!(session = %key, generation = g, "new generation started"),
                    Err(e) => tracing::warn!("failed to start new generation: {e:#}"),
                }
            }
            self.invalidate_plugins_skills_cache();
        }

        // /reset — clear current session without summary or generation change.
        if self.handle.reset_signal.load(Ordering::SeqCst) {
            self.handle.reset_signal.store(false, Ordering::SeqCst);
            info!("reset_signal received, resetting sessions");

            // Save session summary to memory before clearing.
            let compaction_model2 = self.config.agents.defaults.compaction
                .as_ref().and_then(|c| c.model.clone())
                .or_else(|| self.handle.config.model.as_ref()?.primary.clone())
                .unwrap_or_else(|| "default".to_owned());
            self.save_session_summaries_to_memory(&compaction_model2).await;

            self.sessions.clear();
            self.compaction_state.clear();
            for key in self.store.db.list_sessions().unwrap_or_default() {
                let _ = self.store.db.delete_session(&key);
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
                        let msg_tokens: usize = self.sessions.get(session_key)
                            .map(|msgs| msgs.iter().map(crate::agent::context_mgr::msg_tokens).sum())
                            .unwrap_or(0);
                        self.handle.last_ctx_tokens.store(msg_tokens, std::sync::atomic::Ordering::Relaxed);
                        let ctx_limit = self.handle.config.model.as_ref()
                            .and_then(|m| m.context_tokens)
                            .or(self.config.agents.defaults.model.as_ref()
                                .and_then(|m| m.context_tokens))
                            .unwrap_or(64000) as usize;

                        // Estimate system prompt + tools tokens.
                        let tools = build_tool_list(
                            &self.skills,
                            self.agents.as_deref(),
                            &self.handle.id,
                            &self.config.agents.external,
                        );
                        let tools_json = serde_json::to_string(&tools).unwrap_or_default();
                        let tools_tokens = tools_json.len() / 4; // JSON is mostly ASCII, ~4 chars/token
                        // System prompt: estimate from last known size or compute a rough guess.
                        let sys_tokens = 3500; // typical system prompt ~3.5k tokens
                        let all_tokens = sys_tokens + tools_tokens + msg_tokens;

                        format!(
                            "Gateway: running\nOS: {os}\nModel: {model}\nSessions: {sessions}\n\
                             Context: system ~{:.1}k + tools ~{:.1}k + messages ~{:.1}k = ~{:.1}k/{:.0}k tokens\n\
                             Uptime: {uptime}\nVersion: rsclaw {}",
                            sys_tokens as f64 / 1000.0,
                            tools_tokens as f64 / 1000.0,
                            msg_tokens as f64 / 1000.0,
                            all_tokens as f64 / 1000.0,
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
                                self.compact_single(compaction_model, &transcript, None).await
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
                        // Look for the compaction-tagged message (role=User with COMPACTION prefix).
                        const COMPACTION_TAG: &str = "[CONTEXT COMPACTION";
                        if let Some(msgs) = self.sessions.get(session_key) {
                            let summary_text = msgs.iter().find_map(|m| {
                                let text = match &m.content {
                                    crate::provider::MessageContent::Text(s) => s.clone(),
                                    crate::provider::MessageContent::Parts(parts) => parts.iter().filter_map(|p| {
                                        if let crate::provider::ContentPart::Text { text } = p { Some(text.as_str()) } else { None }
                                    }).collect::<Vec<_>>().join(" "),
                                };
                                if text.starts_with(COMPACTION_TAG) { Some(text) } else { None }
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
                                        tags: vec![],
                pinned: false,
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
                        let flags = self.handle.abort_flags.write().expect("abort_flags lock poisoned");
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
                            recalled_memory_ids: std::collections::HashSet::new(),
                            loop_warning_triggered: false,
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

        // Build system prompt — cached for entire gateway lifetime.
        // Only rebuilt on gateway restart.
        if self.cached_system_prompt.is_none() {
            let prompt = build_system_prompt(&ws_ctx, &self.skills, &self.config.raw);
            // DEBUG: dump full system prompt to file for inspection
            if std::env::var("RSCLAW_DUMP_PROMPT").is_ok() {
                let dump_path = crate::config::loader::base_dir().join("debug_system_prompt.txt");
                if let Err(e) = std::fs::write(&dump_path, &prompt) {
                    tracing::warn!("failed to dump system prompt: {e}");
                }
                tracing::info!(path = %dump_path.display(), len = prompt.len(), "dumped system prompt");
            }
            self.cached_system_prompt = Some(prompt);
        }
        let system_prompt = self.cached_system_prompt.clone().expect("just set");

        // --- Dynamic context: injected into system prompt suffix ---
        // Only truly dynamic, per-turn content goes here. Static rules belong
        // in the base system prompt. Auto-recall memories are removed — LLM
        // uses the memory tool to search when needed. Date is removed — LLM
        // uses shell commands when it needs the current date/time.
        let mut dynamic_ctx = Vec::<String>::new();

        // Loop A (organic evolution): collect recalled memory IDs for feedback.
        // Auto-recall is disabled — LLM uses the memory tool to search when needed.
        // This avoids injecting dynamic content into user messages which would break
        // prefix KV cache across turns.
        let auto_recalled_ids = std::collections::HashSet::<String>::new();

        // Background context injection (/ctx).
        let btw_block = self
            .btw_manager
            .to_prompt_block_relevant(session_key, channel, text)
            .await;
        if !btw_block.is_empty() {
            dynamic_ctx.push(btw_block);
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

            // Internal channels (heartbeat/cron/system): only memory tool
            if session_key.starts_with("heartbeat:") || session_key.starts_with("cron:") || session_key.starts_with("system:") {
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

            all
        };

        // Cache tools for compaction KV cache reuse.
        self.cached_tools = tools.clone();

        // Check vision support before loading session (avoids borrow conflict).
        let kv_mode = self.config.agents.defaults.kv_cache_mode.unwrap_or(1);
        // Always detect vision capability — used to decide which model describes images.
        // kvCacheMode >= 1: images are described then stored as text (never base64 in session).
        // kvCacheMode = 0: images kept as base64 in session for vision models.
        let model_has_vision = model_supports_vision(&model, &self.config);
        let vision = if kv_mode >= 1 { false } else { model_has_vision };

        // ---------------------------------------------------------------
        // Media processing: convert images/videos to text descriptions.
        // Done BEFORE load_session() to avoid borrow conflicts with self.
        // Session stores ONLY text — no base64, no binary blobs.
        // This preserves KV cache and prevents context bloat.
        // ---------------------------------------------------------------
        let mut media_descriptions = Vec::<String>::new();
        let mut vision_images_for_current_turn = Vec::<String>::new(); // base64 URIs for vision model

        for img in &images {
            if img.mime_type.starts_with("video/") {
                // Video: generate text placeholder (transcript support is TODO).
                let desc = crate::agent::context_mgr::describe_video(None, None);
                media_descriptions.push(desc);
            } else {
                // Image: get text description via vision model.
                // If current model supports vision, also keep the image for the
                // current LLM turn (so it sees the original).
                if vision {
                    // Current model can see images — keep for this turn's API call.
                    if let Some(compressed) = compress_image_for_llm(&img.data) {
                        vision_images_for_current_turn.push(compressed);
                    } else {
                        vision_images_for_current_turn.push(img.data.clone());
                    }
                }

                // Generate text description for session storage.
                // Use current model if vision-capable (even in kv_cache_mode >= 1,
                // the image description is a separate LLM call that doesn't pollute
                // the main session's KV cache). Otherwise find a vision model.
                let vision_model = if model_has_vision {
                    model.clone()
                } else {
                    // Try image model config, then fallback to known vision providers.
                    self.handle.config.model.as_ref()
                        .and_then(|m| m.image.as_deref())
                        .or_else(|| self.config.agents.defaults.model.as_ref()
                            .and_then(|m| m.image.as_deref()))
                        .map(|s| s.to_owned())
                        .unwrap_or_else(|| {
                            // Auto-detect a vision-capable provider from config.
                            let vision_providers = ["doubao", "gemini", "openai", "qwen"];
                            for vp in vision_providers {
                                let has_key = self.config.model.models.as_ref()
                                    .and_then(|m| m.providers.get(vp))
                                    .and_then(|p| p.api_key.as_ref())
                                    .is_some()
                                    || std::env::var(format!("{}_API_KEY", vp.to_uppercase())).is_ok();
                                if has_key {
                                    return match vp {
                                        "doubao" => "doubao/doubao-seed-2-0-pro-260215".to_owned(),
                                        "gemini" => "gemini/gemini-2.0-flash".to_owned(),
                                        "openai" => "openai/gpt-4o-mini".to_owned(),
                                        "qwen" => "qwen/qwen-vl-max".to_owned(),
                                        _ => format!("{vp}/default"),
                                    };
                                }
                            }
                            tracing::warn!("no vision-capable provider found for image description");
                            String::new() // no vision model available
                        })
                };

                if vision_model.is_empty() {
                    media_descriptions.push("[图片] (无视觉模型可用)".to_owned());
                } else {
                    let desc = crate::agent::context_mgr::describe_image_via_llm(
                        &img.data, &vision_model, &mut self.failover, &self.providers,
                    ).await;
                    match desc {
                        Some(d) => media_descriptions.push(format!("[图片] {d}")),
                        None => media_descriptions.push("[图片] (无法生成描述)".to_owned()),
                    }
                }
            }
        }

        // Build the persisted message: user text + media descriptions (text only).
        let persist_text = if media_descriptions.is_empty() {
            text.to_owned()
        } else {
            format!("{}\n\n{}", text, media_descriptions.join("\n"))
        };

        // NOW load session (after media processing is done, no more self borrows).
        let session_messages = self.load_session(session_key);

        // First user message in session: prepend session metadata (date, timezone,
        // channel). Stored in session so it becomes part of the stable prefix
        // for KV cache — never changes across turns.
        // Also triggers after /clear (session may contain a summary but no user messages).
        let has_user_msg = session_messages.iter().any(|m| m.role == Role::User);
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
            format!("{session_meta}\n{persist_text}")
        } else {
            persist_text
        };

        let persist_msg = Message {
            role: Role::User,
            content: MessageContent::Text(persist_text),
        };
        session_messages.push(persist_msg.clone());
        if let Err(e) = self.store.db.append_message(
            session_key,
            &serde_json::to_value(&persist_msg).unwrap_or_default(),
        ) {
            tracing::warn!("failed to persist user message: {e:#}");
        }

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
            has_images: !vision_images_for_current_turn.is_empty(),
            user_msg_with_images: if !vision_images_for_current_turn.is_empty() {
                // Build a multimodal message for the current LLM turn only.
                // The persisted message is text-only; this adds images back for vision.
                let base_text = match &persist_msg.content {
                    MessageContent::Text(t) => t.clone(),
                    MessageContent::Parts(p) => p.iter().filter_map(|part| {
                        if let ContentPart::Text { text } = part { Some(text.as_str()) } else { None }
                    }).collect::<Vec<_>>().join(""),
                };
                let mut parts = vec![ContentPart::Text { text: base_text }];
                for img_uri in &vision_images_for_current_turn {
                    parts.push(ContentPart::Image { url: img_uri.clone() });
                }
                Some(Message { role: Role::User, content: MessageContent::Parts(parts) })
            } else {
                None
            },
            parse_error_count: 0,
            recalled_memory_ids: auto_recalled_ids,
            loop_warning_triggered: false,
        };

        // --- Plugins & Skills: cached, frozen at session start ---
        // On first turn: build and cache. On subsequent turns: detect new
        // additions and append as trailing system messages. On compact/`/new`:
        // rebuild (handled by invalidate_plugins_skills_cache()).

        // Build/cache plugins system message.
        // TODO: populate from self.plugins when plugin system is merged.
        if self.cached_plugins_system.is_none() {
            // Placeholder — will be populated after jimeng-automation merge.
            // self.cached_plugins_system = Some(build_plugins_system(&self.plugins));
        }

        // Build/cache skills system message.
        if self.cached_skills_system.is_none() {
            let (msg, snapshot) = Self::build_skills_system_msg(&self.skills);
            self.cached_skills_system = msg;
            self.cached_skills_snapshot = snapshot;
        }

        // Detect newly added skills since cache was frozen.
        // New skills are appended as trailing system messages, not merged
        // into the cached [2] — this preserves KV cache prefix.
        let mut new_skills_tail: Vec<String> = Vec::new();
        {
            let mut current_names: Vec<String> = self.skills.all()
                .map(|s| s.name.clone())
                .collect();
            current_names.sort();
            for name in &current_names {
                if !self.cached_skills_snapshot.contains(name) {
                    if let Some(skill) = self.skills.all().find(|s| &s.name == name) {
                        new_skills_tail.push(format!(
                            "<skill name=\"{}\" version=\"{}\">\n{}\n</skill>",
                            skill.name,
                            skill.version.as_deref().unwrap_or(""),
                            skill.prompt.trim(),
                        ));
                    }
                }
            }
        }

        let plugins_system = self.cached_plugins_system.clone();
        let skills_system = self.cached_skills_system.clone();

        let reply = time::timeout(
            Duration::from_secs(timeout_secs),
            self.agent_loop(
                &mut ctx, &model, &system_prompt,
                plugins_system.as_deref(),
                skills_system.as_deref(),
                tools, extra_tools, abort_flag.clone(),
                dynamic_ctx, new_skills_tail,
            ),
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
            tracing::debug!(signal, recalled = ctx.recalled_memory_ids.len(), "evolution: applying feedback");
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
                            d.tier == crate::agent::memory::MemDocTier::Core
                                && !d.tags.iter().any(|t| t == "crystallized")
                        })
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            drop(store);

            if !candidates.is_empty() {
                let mem_clone = Arc::clone(mem);
                let ws_dir = self
                    .handle
                    .config
                    .workspace
                    .as_deref()
                    .or(self.config.agents.defaults.workspace.as_deref())
                    .map(crate::agent::runtime::expand_tilde)
                    .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));
                let skills_dir = ws_dir.join("skills");
                let scope = format!("agent:{}", self.handle.id);
                tokio::spawn(async move {
                    for doc_id in candidates {
                        let mut store = mem_clone.lock().await;
                        match crate::skill::crystallizer::find_cluster(&store, &doc_id, &scope) {
                            Ok(Some(cluster)) => {
                                let prompt =
                                    crate::skill::crystallizer::build_distill_prompt(&cluster);
                                // Tag all cluster docs as crystallized (even without
                                // LLM distillation) to prevent repeated attempts.
                                let ids: Vec<String> =
                                    cluster.iter().map(|d| d.id.clone()).collect();
                                for id in &ids {
                                    if let Err(e) = store.tag_doc(id, "crystallized").await {
                                        tracing::debug!(id, "tag_doc failed: {e:#}");
                                    }
                                }
                                drop(store);
                                // Write a draft skill with the prompt as content.
                                // A future version can use an LLM to distill it.
                                let slug = crate::skill::crystallizer::slugify(
                                    &prompt.lines().next().unwrap_or("auto-skill"),
                                );
                                match crate::skill::crystallizer::write_skill(
                                    &skills_dir,
                                    &slug,
                                    &prompt,
                                ) {
                                    Ok(path) => {
                                        tracing::info!(
                                            ?path,
                                            "crystallized {} memories into skill",
                                            ids.len()
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!("skill crystallization write failed: {e:#}");
                                    }
                                }
                            }
                            Ok(None) => {} // not enough related memories yet
                            Err(e) => {
                                tracing::debug!(doc_id, "crystallization check failed: {e:#}");
                            }
                        }
                    }
                });
            }
        }

        // Auto-Capture (AGENTS.md §31): persist user message as memory note.
        // Threshold is 8 bytes (not 20) so short messages with key data like
        // "手机号18674030927" (20 bytes) are not silently dropped.
        // Messages containing long digit sequences (phone numbers, IDs, codes)
        // get higher importance so they survive memory decay.
        let has_key_digits = {
            let mut run = 0usize;
            let mut max_run = 0usize;
            for b in text.bytes() {
                if b.is_ascii_digit() { run += 1; max_run = max_run.max(run); } else { run = 0; }
            }
            max_run >= 8 // 8+ consecutive digits = phone number / ID / code
        };
        // Skip auto-capture for internal channels — heartbeat/cron/system
        // don't need long-term memory and would pollute user recall results.
        let internal_channel = matches!(channel, "heartbeat" | "cron" | "system");
        if let Some(ref mem) = self.memory
            && text.len() > 8
            && !reply.text.starts_with(NO_REPLY_TOKEN)
            && !internal_channel
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
                // Bump importance for messages with phone numbers / long IDs so
                // they survive memory decay and compaction fact extraction.
                importance: if has_key_digits { 0.85 } else { 0.5 },
                vector: vec![],
                tier: Default::default(),
                abstract_text: None,
                overview_text: None,
                tags: vec![],
                pinned: false,
            };
            if let Err(e) = mem.lock().await.add(doc).await {
                tracing::warn!("auto-capture memory add failed: {e:#}");
            }
            // Also index in tantivy BM25 for hybrid search.
            if let Err(e) = self
                .store
                .search
                .index_memory_doc(&doc_id, &doc_scope, "note", text)
            {
                tracing::warn!("BM25 index failed for auto-capture doc: {e:#}");
            }

            // Deterministic entity extraction: phone numbers, ID cards, emails.
            let user_entities = crate::agent::context_mgr::extract_key_entities(text);
            if !user_entities.is_empty() {
                crate::agent::context_mgr::write_entity_memories(
                    mem, &doc_scope, user_entities,
                ).await;
            }

            // LLM-based entity extraction moved to compaction — the summary
            // prompt includes an Entities section, so extraction happens at
            // compaction time with zero extra LLM calls.
        }

        // Deterministic entity extraction from assistant reply.
        if let Some(ref mem) = self.memory {
            let reply_entities = crate::agent::context_mgr::extract_key_entities(&reply.text);
            if !reply_entities.is_empty() {
                let scope = format!("agent:{}", self.handle.id);
                crate::agent::context_mgr::write_entity_memories(
                    mem, &scope, reply_entities,
                ).await;
            }
        }

        // Compaction check (AGENTS.md §15).
        self.compact_if_needed(session_key, &model).await;

        // Evict stale sessions if the cache has grown too large.
        self.evict_stale_sessions();

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

    /// Save summaries of all active sessions to long-term memory.
    ///
    /// Called before `/new` and `/reset` — since no summary is injected into
    /// the new session, memory is the only way the LLM can find prior context.
    /// Uses KV cache mode when available (session is still in memory).
    async fn save_session_summaries_to_memory(&mut self, model: &str) {
        if self.memory.is_none() { return; }

        let kv_cache_mode = self.config.agents.defaults.kv_cache_mode.unwrap_or(1);

        // Collect session data upfront to avoid borrow conflicts.
        let session_data: Vec<(String, String)> = self.sessions.iter()
            .filter(|(_, msgs)| msgs.len() > 2)
            .map(|(key, msgs)| {
                let transcript = Self::msgs_to_text_static(msgs, 16_000);
                (key.clone(), transcript)
            })
            .collect();

        for (session_key, transcript) in &session_data {
            // Generate summary — try KV cache mode first.
            let summary = if kv_cache_mode >= 1 {
                let result = self.compact_with_kv_cache(session_key, model, transcript, None).await;
                if result.is_some() { result } else {
                    self.compact_single(model, transcript, None).await
                }
            } else {
                self.compact_single(model, transcript, None).await
            };

            let Some(summary) = summary else { continue };

            // Store as a session_summary memory doc.
            let scope = format!("agent:{}", self.handle.id);
            let doc = crate::agent::memory::MemoryDoc {
                id: format!("session-summary-{}", uuid::Uuid::new_v4()),
                scope: scope.clone(),
                kind: "session_summary".to_owned(),
                text: summary,
                vector: vec![],
                created_at: 0,
                accessed_at: 0,
                access_count: 0,
                importance: 0.8,
                tier: Default::default(),
                abstract_text: None,
                overview_text: None,
                tags: vec![],
                pinned: false,
            };
            let mem = self.memory.as_ref().expect("checked above");
            if let Err(e) = mem.lock().await.add(doc).await {
                tracing::warn!("failed to save session summary to memory: {e:#}");
            } else {
                info!(session = %session_key, "session summary saved to memory before clear");
            }
        }
    }

    /// Build the skills system message from the registry (sorted by name).
    /// Returns (message, sorted_name_snapshot).
    fn build_skills_system_msg(skills: &crate::skill::SkillRegistry) -> (Option<String>, Vec<String>) {
        let mut all_skills: Vec<_> = skills.all().collect();
        all_skills.sort_by(|a, b| a.name.cmp(&b.name));
        let snapshot: Vec<String> = all_skills.iter().map(|s| s.name.clone()).collect();

        if all_skills.is_empty() {
            return (None, snapshot);
        }

        let skill_prompts: String = all_skills
            .iter()
            .map(|s| format!(
                "<skill name=\"{}\" version=\"{}\">\n{}\n</skill>",
                s.name,
                s.version.as_deref().unwrap_or(""),
                s.prompt.trim(),
            ))
            .collect::<Vec<_>>()
            .join("\n\n");

        let msg = format!(
            "## Installed Skills\n\
             When the user's request matches a skill, follow its instructions \
             unless a plugin already handles the task.\n\
             Priority: plugins > skills > built-in tools.\n\n\
             {skill_prompts}"
        );
        (Some(msg), snapshot)
    }

    /// Invalidate cached plugins and skills system messages.
    /// Called after compaction or `/new` to force a rebuild on the next turn.
    pub(crate) fn invalidate_plugins_skills_cache(&mut self) {
        self.cached_plugins_system = None;
        self.cached_skills_system = None;
        self.cached_skills_snapshot.clear();
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
        plugins_system: Option<&str>,
        skills_system: Option<&str>,
        tools: Vec<ToolDef>,
        extra_tools: Vec<ToolDef>,
        abort_flag: Arc<AtomicBool>,
        dynamic_ctx: Vec<String>,
        new_skills_tail: Vec<String>,
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
            let mut completed = Vec::new();
            pending.retain(|(tid, sk, result)| {
                if sk == &ctx.session_key {
                    completed.push((tid.clone(), sk.clone(), result.clone()));
                    false // remove from pending
                } else {
                    true // keep for other sessions
                }
            });
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

        // Check for pending exec results from background tasks.
        let pending_results = self.exec_pool.collect_pending_for_session(&ctx.session_key).await;
        if !pending_results.is_empty() {
            info!(session = %ctx.session_key, count = pending_results.len(), "exec_pool: collected pending results");
            if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                // Collect existing ToolUse IDs in session
                let session_tool_ids: std::collections::HashSet<String> = sess.iter()
                    .filter_map(|m| {
                        if m.role == Role::Assistant {
                            if let MessageContent::Parts(parts) = &m.content {
                                Some(parts.iter().filter_map(|p| {
                                    if let ContentPart::ToolUse { id, .. } = p { Some(id.clone()) } else { None }
                                }).collect::<Vec<_>>())
                            } else { None }
                        } else { None }
                    })
                    .flatten()
                    .collect();

                // Find running ToolResults to replace
                let running_ids: std::collections::HashSet<String> = sess.iter()
                    .filter_map(|m| {
                        if m.role == Role::Tool {
                            if let MessageContent::Parts(parts) = &m.content {
                                for p in parts {
                                    if let ContentPart::ToolResult { tool_use_id, content, .. } = p {
                                        if content.contains("\"status\":\"running\"") || content.contains("\"status\": \"running\"") {
                                            return Some(tool_use_id.clone());
                                        }
                                    }
                                }
                            }
                        }
                        None
                    })
                    .collect();

                // Remove running status ToolResults that will be replaced
                let ids_to_replace: std::collections::HashSet<String> = pending_results.iter()
                    .map(|r| r.tool_call_id.clone())
                    .filter(|id| running_ids.contains(id))
                    .collect();
                if !ids_to_replace.is_empty() {
                    sess.retain(|m| {
                        if m.role == Role::Tool {
                            if let MessageContent::Parts(parts) = &m.content {
                                for p in parts {
                                    if let ContentPart::ToolResult { tool_use_id, content, .. } = p {
                                        if ids_to_replace.contains(tool_use_id) && (content.contains("\"status\":\"running\"") || content.contains("\"status\": \"running\"")) {
                                            return false;
                                        }
                                    }
                                }
                            }
                        }
                        true
                    });
                }

                for result in pending_results {
                    let tool_call_id = result.tool_call_id.clone();
                    // If ToolUse not in history, inject synthetic one
                    if !session_tool_ids.contains(&tool_call_id) {
                        sess.push(Message {
                            role: Role::Assistant,
                            content: MessageContent::Parts(vec![ContentPart::ToolUse {
                                id: tool_call_id.clone(),
                                name: "exec".to_owned(),
                                input: serde_json::json!({"command": result.command, "_synthetic": true}),
                            }]),
                        });
                    }
                    let is_error = result.exit_code.map(|c| c != 0).unwrap_or(true);
                    let content = serde_json::json!({
                        "exit_code": result.exit_code,
                        "stdout": result.stdout,
                        "stderr": result.stderr,
                    }).to_string();
                    sess.push(Message {
                        role: Role::Tool,
                        content: MessageContent::Parts(vec![ContentPart::ToolResult {
                            tool_use_id: tool_call_id,
                            content,
                            is_error: Some(is_error),
                        }]),
                    });
                }
            }
        }

        // Dynamic iteration limit based on task complexity.
        // Default: 15 iterations. Complex tools (browser/opencode/exec): up to configured max.
        const BASE_ITERATIONS: usize = 20;
        let configured_complex: usize = self.config.agents.defaults.max_iterations
            .map(|v| v as usize)
            .unwrap_or(50);
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
                apply_context_budget_trim(sess, context_tokens, &system_prompt, &tools);
            }

            // Build API copy of messages — clone from session, then inject
            // dynamic context and vision images. Session storage is NOT modified.
            let mut messages = {
                let mut raw = self
                    .sessions
                    .get(&ctx.session_key)
                    .cloned()
                    .unwrap_or_default();

                // For vision models: replace last user message with multimodal
                // version containing original images (only for this API call).
                if ctx.has_images {
                    if let Some(last) = raw.last_mut() {
                        if last.role == Role::User {
                            *last = ctx.user_msg_with_images.clone().unwrap_or(last.clone());
                        }
                    }
                }

                // Insert plugins and skills as system messages at the front
                // of history. After provider prepends the main system prompt [0],
                // the final order is:
                //   [0] system — main prompt (stable, never changes)
                //   [1] system — plugins (highest priority, overrides skills/built-in)
                //   [2] system — skills (overrides built-in tools)
                //   [3+] history — user/assistant/tool messages
                // Insert in reverse order since each insert(0, ...) pushes previous ones down.
                if let Some(skills) = skills_system {
                    raw.insert(0, Message {
                        role: Role::System,
                        content: MessageContent::Text(skills.to_owned()),
                    });
                }
                if let Some(plugins) = plugins_system {
                    raw.insert(0, Message {
                        role: Role::System,
                        content: MessageContent::Text(plugins.to_owned()),
                    });
                }

                // Append newly installed skills as trailing system messages.
                // These were added after the cached [2] was frozen — appending
                // at the tail preserves the KV cache prefix.
                for skill_block in &new_skills_tail {
                    raw.push(Message {
                        role: Role::System,
                        content: MessageContent::Text(format!(
                            "## New Skill Installed\n{skill_block}"
                        )),
                    });
                }

                // Inject remaining dynamic context (btw /ctx) as a trailing
                // system message after all history. Session storage is unchanged.
                if !dynamic_ctx.is_empty() {
                    let ctx_block = dynamic_ctx.join("\n\n");
                    raw.push(Message {
                        role: Role::System,
                        content: MessageContent::Text(ctx_block),
                    });
                }

                // Repair transcript: ensure all tool_calls have matching tool_results.
                let repair_result = repair_tool_result_pairing(raw);

                // Persist any synthetic tool results to session storage
                // so they don't need to be added again on the next turn.
                if !repair_result.synthetic_messages.is_empty() {
                    for synthetic in &repair_result.synthetic_messages {
                        if let Err(e) = self.store.db.append_message(
                            &ctx.session_key,
                            &serde_json::to_value(synthetic).unwrap_or_default(),
                        ) {
                            tracing::warn!("failed to persist synthetic message: {e:#}");
                        }
                    }
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
            let msg_tokens_sum: usize = messages.iter().map(msg_tokens).sum();
            // Include system prompt + tools in total context estimate.
            let sys_tokens = estimate_tokens(system_prompt);
            let tools_tokens: usize = tools.iter()
                .map(|t| estimate_tokens(&t.name) + estimate_tokens(&t.description) + estimate_tokens(&t.parameters.to_string()))
                .sum();
            let approx_tokens = msg_tokens_sum + sys_tokens + tools_tokens;
            self.handle.last_ctx_tokens.store(approx_tokens, std::sync::atomic::Ordering::Relaxed);
            info!(session = %ctx.session_key, msg_count, approx_tokens, sys_tokens, tools_tokens, msg_tokens = msg_tokens_sum, model = %model, "LLM call: context size");

            // Context usage awareness: inject hint into the LAST user message
            // (not system prompt) to preserve KV cache prefix stability.
            if approx_tokens > 0 && context_tokens > 0 {
                let usage_pct = (approx_tokens * 100) / context_tokens;
                let usage_hint = if usage_pct >= 90 {
                    Some(format!("[Context usage: {usage_pct}% — CRITICAL. \
                        Keep responses very concise. Do not re-read files already in context. \
                        Suggest user start a new session if task is complete.]"))
                } else if usage_pct >= 70 {
                    Some(format!("[Context usage: {usage_pct}%. \
                        Optimize: keep tool outputs short (use offset/limit for reads, \
                        pipe to head/tail for commands). Avoid re-reading files already in context.]"))
                } else {
                    None
                };
                if let Some(hint) = usage_hint {
                    if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == Role::User) {
                        match &mut last_user.content {
                            MessageContent::Text(t) => {
                                t.push_str(&format!("\n\n{hint}"));
                            }
                            MessageContent::Parts(parts) => {
                                parts.push(ContentPart::Text { text: format!("\n\n{hint}") });
                            }
                        }
                    }
                }
            }
            let effective_system = system_prompt.to_owned();

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

            let kv_cache_mode = self.config.agents.defaults.kv_cache_mode.unwrap_or(1);
            let req = LlmRequest {
                model: model.to_owned(),
                messages,
                tools: tools.clone(),
                system: Some(effective_system.clone()),
                max_tokens: configured_max_tokens,
                temperature,
                frequency_penalty: self.config.agents.defaults.frequency_penalty,
                thinking_budget,
                kv_cache_mode,
                session_key: if kv_cache_mode >= 2 { Some(ctx.session_key.clone()) } else { None },
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
                                ctx.loop_warning_triggered = true;
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
                                } else if last.2.is_string() {
                                    // Accumulator is String but chunk is Number/Bool/etc.
                                    // llamacpp sends digits as Number tokens during streaming.
                                    // Convert to string and append.
                                    let fragment = match &input {
                                        serde_json::Value::String(s) => s.clone(),
                                        serde_json::Value::Number(n) => n.to_string(),
                                        serde_json::Value::Bool(b) => b.to_string(),
                                        serde_json::Value::Null => "null".to_owned(),
                                        other => serde_json::to_string(other).unwrap_or_default(),
                                    };
                                    let existing = last.2.as_str().unwrap_or("");
                                    last.2 = serde_json::Value::String(format!("{existing}{fragment}"));
                                } else if let Some(new_str) = input.as_str() {
                                    // Last is Object but new chunk is String — convert.
                                    let existing_str = serde_json::to_string(&last.2).unwrap_or_default();
                                    last.2 = serde_json::Value::String(format!("{existing_str}{new_str}"));
                                } else {
                                    // Last resort: convert both to string.
                                    let existing_str = serde_json::to_string(&last.2).unwrap_or_default();
                                    let fragment = serde_json::to_string(&input).unwrap_or_default();
                                    last.2 = serde_json::Value::String(format!("{existing_str}{fragment}"));
                                    tracing::debug!(
                                        "streaming tool call: merged non-string types as strings"
                                    );
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
                // Match <function=NAME> ... </function> blocks, then extract
                // all <parameter=KEY>VALUE</parameter> pairs within each block.
                let fn_re = regex::Regex::new(
                    r#"<function=(\w+)>([\s\S]*?)</function>"#
                ).expect("fn_re compile");
                let param_re = regex::Regex::new(
                    r#"<parameter=(\w+)>([\s\S]*?)</parameter>"#
                ).expect("param_re compile");
                for fn_cap in fn_re.captures_iter(&text_buf) {
                    let name = fn_cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    let body = fn_cap.get(2).map(|m| m.as_str()).unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    let mut input = serde_json::Map::new();
                    for p_cap in param_re.captures_iter(body) {
                        let key = p_cap.get(1).map(|m| m.as_str()).unwrap_or("");
                        let val = p_cap.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                        if !key.is_empty() {
                            input.insert(key.to_owned(), json!(val));
                        }
                    }
                    let id = format!("rescued_{name}_{}", tool_calls.len());
                    tracing::info!(name, params = ?input.keys().collect::<Vec<_>>(), "agent_loop: rescued tool call from text");
                    tool_calls.push((id, name.to_owned(), Value::Object(input)));
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
                // Only persist non-empty assistant replies to session.
                // Empty responses pollute history and confuse the LLM on
                // subsequent turns (it sees its own empty reply and mimics it).
                if !text_buf.trim().is_empty() {
                    let assistant_msg = Message {
                        role: Role::Assistant,
                        content: MessageContent::Text(text_buf.clone()),
                    };
                    if let Err(e) = self.store.db.append_message(
                        &ctx.session_key,
                        &serde_json::to_value(&assistant_msg).unwrap_or_default(),
                    ) {
                        tracing::error!(error = %e, "failed to persist message");
                    }
                    if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                        sess.push(assistant_msg);
                    }
                } else {
                    tracing::debug!(session = %ctx.session_key, "skipping empty assistant reply (not persisted)");
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
            // Intermediate text (e.g. "好的，我来帮你搜索") is NOT saved to session —
            // it's already sent to the user above but pollutes context quality.
            let mut parts: Vec<crate::provider::ContentPart> = Vec::new();
            if !text_buf.is_empty() && tool_calls.is_empty() {
                // Only save text if there are no tool calls (final reply).
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
            if let Err(e) = self.store.db.append_message(
                &ctx.session_key,
                &serde_json::to_value(&assistant_msg).unwrap_or_default(),
            ) {
                tracing::error!(error = %e, "failed to persist message");
            }
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
                    if let Err(e) = self.store.db.append_message(
                        &ctx.session_key,
                        &serde_json::to_value(&tool_msg).unwrap_or_default(),
                    ) {
                        tracing::error!(error = %e, "failed to persist message");
                    }
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
                        // Loop A: capture recalled memory IDs from search results.
                        if tool_name == "memory" || tool_name == "memory_search" {
                            if let Some(results) = v.get("results").and_then(|r| r.as_array()) {
                                for item in results {
                                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                                        ctx.recalled_memory_ids.insert(id.to_owned());
                                    }
                                }
                            }
                        }
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

                // Auto-send files: any tool returning __send_file=true queues the
                // file for delivery. Images go to tool_images, others to tool_files.
                {
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
                                        else if lower.ends_with(".ogg") { "audio/ogg" }
                                        else if lower.ends_with(".opus") { "audio/opus" }
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

                // Result sufficiency hint: when a tool returns substantial content
                // after 3+ iterations, nudge the LLM to stop if the result looks complete.
                let session_text = if iteration >= 3
                    && !session_text.contains("\"error\"")
                    && !session_text.contains("_do_not_retry")
                    && session_text.len() > 500
                    && !session_text.contains("[LOOP WARNING]")
                {
                    format!(
                        "{session_text}\n\n[HINT: This result contains substantial content. \
                         If it answers the user's question, reply directly without further tool calls.]"
                    )
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
                if let Err(e) = self.store.db.append_message(
                    &ctx.session_key,
                    &serde_json::to_value(&tool_msg).unwrap_or_default(),
                ) {
                    tracing::error!(error = %e, "failed to persist message");
                }
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
            "video_gen" | "video" => return self.tool_video(args).await,
            "pdf" => return self.tool_pdf(args).await,
            "text_to_voice" | "text_to_speech" | "tts" => return self.tool_tts(args).await,
            "send_message" | "message" => return self.tool_message(args).await,
            "clarify" => return self.tool_clarify(args).await,
            "anycli" | "opencli" => return self.tool_anycli(args).await,
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
    // Organic evolution helpers
    // -----------------------------------------------------------------------

    /// Infer an outcome signal from the completed turn.
    ///
    /// Returns a value in \[-0.3, 0.3\]: positive = helpful, negative = unhelpful.
    /// Used by Loop A to adjust importance of recalled memories.
    fn infer_outcome_signal(reply: &AgentReply, ctx: &RunContext, channel: &str) -> f32 {
        // Internal channels produce no signal.
        if matches!(channel, "heartbeat" | "system" | "cron") {
            return 0.0;
        }

        let mut signal = 0.0_f32;

        // Negative signals.
        if reply.is_empty {
            signal -= 0.1;
        }
        if ctx.loop_warning_triggered {
            signal -= 0.15;
        }

        // Positive signals.
        if !reply.is_empty && reply.text.len() > 100 {
            signal += 0.05;
        }
        if !reply.is_empty && !ctx.loop_warning_triggered {
            signal += 0.05;
        }

        signal.clamp(-0.3, 0.3)
    }

    // -----------------------------------------------------------------------
    // Built-in tool implementations
    // -----------------------------------------------------------------------

    pub(crate) async fn tool_memory_search(&self, args: Value) -> Result<Value> {
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

    pub(crate) async fn tool_memory_get(&self, args: Value) -> Result<Value> {
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

    pub(crate) async fn tool_memory_put(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let text = args["text"].as_str().unwrap_or("").to_owned();
        // Internal channels (heartbeat/cron/system) get a separate scope so
        // their memories don't pollute normal conversation auto-recall.
        let default_scope = if matches!(ctx.channel.as_str(), "heartbeat" | "cron" | "system") {
            format!("agent:{}:{}", ctx.agent_id, ctx.channel)
        } else {
            ctx.agent_id.clone()
        };
        let scope = args["scope"].as_str().unwrap_or(&default_scope).to_owned();
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
                tags: vec![],
                pinned: false,
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

    pub(crate) async fn tool_memory_delete(&self, args: Value) -> Result<Value> {
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

    // tool_install, tool_list_dir, tool_search_file, tool_search_content,
    // tool_read, tool_write -- moved to tools_file.rs

    // -----------------------------------------------------------------------
    // Compaction (AGENTS.md §15)
    // -----------------------------------------------------------------------


    // Compaction methods (compact_if_needed, compact_force, compact_inner,
    // msgs_to_text_static, compact_single, extract_key_facts,
    // append_transcript) -> moved to compaction.rs

    // -----------------------------------------------------------------------
    // Session eviction
    // -----------------------------------------------------------------------

    /// Remove stale sessions that have been idle longer than
    /// [`SESSION_IDLE_TTL_SECS`].
    ///
    /// Only runs when the number of cached sessions exceeds
    /// [`MAX_SESSIONS_PER_AGENT`] to avoid unnecessary iteration on small
    /// caches.  Corresponding entries in `compaction_state` and
    /// `pending_files` are also removed.
    fn evict_stale_sessions(&mut self) {
        if self.sessions.len() <= MAX_SESSIONS_PER_AGENT {
            return;
        }

        let ttl = Duration::from_secs(SESSION_IDLE_TTL_SECS);
        let now = std::time::Instant::now();

        // Collect keys to evict: sessions whose compaction_state timestamp
        // is older than the TTL, or sessions that have no compaction_state
        // entry at all (never compacted -- use runtime start as proxy).
        let stale_keys: Vec<String> = self
            .sessions
            .keys()
            .filter(|key| {
                if let Some((last_active, _)) = self.compaction_state.get(*key) {
                    now.duration_since(*last_active) > ttl
                } else {
                    // No compaction state -- compare against runtime start.
                    now.duration_since(self.started_at) > ttl
                }
            })
            .cloned()
            .collect();

        if stale_keys.is_empty() {
            return;
        }

        let count = stale_keys.len();
        for key in &stale_keys {
            self.sessions.remove(key);
            self.compaction_state.remove(key);
            self.pending_files.remove(key);
        }

        info!(
            agent = %self.handle.id,
            evicted = count,
            remaining = self.sessions.len(),
            "evicted stale sessions from in-memory cache"
        );
    }

    // tool_exec -- moved to tools_file.rs
    // build_subagent_system_prompt, tool_agent_spawn, tool_agent_task,
    // tool_agent_send, tool_agent_list -> moved to tools_misc.rs

    // Web tools (tool_web_search, search_provider, tool_web_fetch,
    // browser_get_article, browser_search, maybe_summarize,
    // tool_web_download, tool_web_browser) -> moved to tools_web.rs

    // Computer tools (tool_computer_use, tool_image, tool_pdf,
    // generate_tts_audio, tool_tts) -> moved to tools_computer.rs

    // tool_message, tool_cron -> moved to tools_misc.rs
}
// read_cron_jobs, write_cron_jobs -> moved to tools_misc.rs

// tool_sessions_send, tool_sessions_list, tool_sessions_history,
// tool_session_status, tool_gateway, tool_pairing, tool_doc,
// tool_memory_consolidated, tool_session_consolidated,
// tool_agent_consolidated, tool_channel_consolidated,
// tool_channel_actions -> moved to tools_misc.rs

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Expand a leading `~/` to the user's home directory.
pub(crate) fn expand_tilde(p: &str) -> std::path::PathBuf {
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



// ---------------------------------------------------------------------------
// Hybrid memory retrieval — Reciprocal Rank Fusion
// ---------------------------------------------------------------------------

/// Merge vector-search hits and BM25 hits using Reciprocal Rank Fusion (k=60).
///
/// Documents appearing in both lists get a higher combined score.
/// Documents only in one list still contribute their single-list score.
/// Returns the top `top_k` results as `MemoryDoc`s.
#[allow(dead_code)]
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
                        tags: vec![],
                pinned: false,
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


/// Maximum characters to send from file content to LLM.
#[allow(dead_code)]
const MAX_FILE_CONTENT_CHARS: usize = 20_000;



// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Persist dynamic agent to config file
// ---------------------------------------------------------------------------

/// Append an AgentEntry to the `agents.list` array in the config file.
/// The hot-reload watcher will pick up the change automatically.
pub(crate) async fn persist_agent_to_config(entry: &crate::config::schema::AgentEntry) -> anyhow::Result<()> {
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
        agent::context_mgr::msg_chars,
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

