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
/// OpenClaw default is 48 hours (172800s) — matches for complex code
/// generation.
const DEFAULT_TIMEOUT_SECONDS: u64 = 172_800;
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
    "/history", "/cron",
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
    pub loop_detector: LoopDetector,
    /// Whether the current turn includes images.
    pub has_images: bool,
    /// The full user message with image data (for LLM, not persisted).
    pub user_msg_with_images: Option<Message>,
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
        Self {
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
            notification_tx,
            opencode_client: Arc::new(tokio::sync::OnceCell::new()),
            claudecode_client: Arc::new(tokio::sync::OnceCell::new()),
            session_aliases,
        }
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

        // Create session with default model
        let model = std::env::var("OPENCODE_MODEL").ok();
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

        let client = self.get_opencode_client().await?;
        let session_id = client.session_id().await.unwrap_or_default();
        let session_id_clone = session_id.clone();

        // Get notification sender for async result delivery
        let notif_tx = self.notification_tx.clone();
        let target_id = ctx.peer_id.clone();
        let channel_name = ctx.channel.clone();
        let task_str = task.to_string();

        // Send initial notification
        if let Some(ref tx) = notif_tx {
            let _ = tx.send(crate::channel::OutboundMessage {
                target_id: target_id.clone(),
                is_group: false,
                text: "🚀 OpenCode 任务已提交，执行中...".to_string(),
                reply_to: None,
                images: vec![],
                channel: Some(channel_name.clone()),
            });
        }

        // Spawn background task - collect events AND send prompt in parallel
        let notif_tx_bg = notif_tx.clone();
        let target_id_bg = target_id.clone();
        let channel_bg = channel_name.clone();
        tokio::spawn(async move {
            tracing::info!("tool_opencode: background task started");
            // Start event collection FIRST (in parallel with send_prompt)
            let mut event_rx = client.subscribe_events();
            let events = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
            let events_clone = Arc::clone(&events);

            // Event collection task - runs in background
            let notif_tx_clone = notif_tx_bg.clone();
            let target_id_clone = target_id_bg.clone();
            let channel_clone = channel_bg.clone();
            let _event_collector = tokio::spawn(async move {
                let mut pending = String::new();
                let mut interval = 0u64;
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
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
                                        if r.len() > 100 {
                                            format!("✅ {}...", &r[..100])
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
                                _ => String::new(),
                            };

                            if !event_str.is_empty() {
                                events_clone.lock().await.push(event_str.clone());
                                if let Some(ref tx) = notif_tx_clone {
                                    pending.push_str(&event_str);
                                    pending.push('\n');
                                    interval += 1;
                                    if interval >= 3 || pending.len() > 400 {
                                        let _ = tx.send(crate::channel::OutboundMessage {
                                            target_id: target_id_clone.clone(),
                                            is_group: false,
                                            text: format!("🔄 OpenCode\n{}", pending.trim()),
                                            reply_to: None,
                                            images: vec![],
                                            channel: Some(channel_clone.clone()),
                                        });
                                        pending.clear();
                                        interval = 0;
                                    }
                                }
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

            // Process the result
            match send_result {
                Ok(resp) => {
                    tracing::info!(
                        "tool_opencode: send_prompt completed, stop_reason={:?}",
                        resp.stop_reason
                    );

                    let events_text = events.lock().await.join("\n");
                    let collected = client.get_collected_content().await;
                    tracing::info!(
                        "tool_opencode: events_text len={}, collected len={}",
                        events_text.len(),
                        collected.len()
                    );

                    let output = if !events_text.is_empty() {
                        events_text
                    } else if !collected.is_empty() {
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
                        "(无输出)".to_string()
                    };

                    let final_output = if output.len() > 4000 {
                        format!("{}...\n\n[已截断]", &output[..4000])
                    } else {
                        output
                    };

                    // If we got here, it means we have results - send notification to user
                    tracing::info!(
                        "tool_opencode: sending completion notification, output_len={}",
                        final_output.len()
                    );
                    if let Some(ref tx) = notif_tx_bg {
                        match tx.send(crate::channel::OutboundMessage {
                            target_id: target_id_bg.clone(),
                            is_group: false,
                            text: format!("✅ OpenCode 完成\n\n{}", final_output),
                            reply_to: None,
                            images: vec![],
                            channel: Some(channel_bg.clone()),
                        }) {
                            Ok(_) => {
                                tracing::info!("tool_opencode: notification sent successfully")
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
                            text: format!("❌ OpenCode 错误\n\n{}", e),
                            reply_to: None,
                            images: vec![],
                            channel: Some(channel_bg.clone()),
                        });
                    }
                }
            }
            tracing::info!("tool_opencode: background task finished");
            // IMPORTANT: DON'T await event_collector - it runs forever waiting
            // for more events The collected events are already in
            // `events` variable
        });

        Ok(serde_json::json!({
            "output": "OpenCode 任务已提交，完成后将推送结果。",
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
            .or_else(|| std::env::var("CLAUDE_MODEL").ok());
        let session_resp = client.create_session(&cwd, model.as_deref(), None).await?;

        tracing::info!(
            session_id = %session_resp.session_id,
            "Claude Code session created"
        );

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

        let client = self.get_claudecode_client().await?;
        let session_id = client.session_id().await.unwrap_or_default();
        let session_id_clone = session_id.clone();

        // Get notification sender for async result delivery
        let notif_tx = self.notification_tx.clone();
        let target_id = ctx.peer_id.clone();
        let channel_name = ctx.channel.clone();
        let task_str = task.to_string();

        // Send initial notification
        if let Some(ref tx) = notif_tx {
            let _ = tx.send(crate::channel::OutboundMessage {
                target_id: target_id.clone(),
                is_group: false,
                text: "🚀 Claude Code 任务已提交，执行中...".to_string(),
                reply_to: None,
                images: vec![],
                channel: Some(channel_name.clone()),
            });
        }

        // Spawn background task - collect events AND send prompt in parallel
        let notif_tx_bg = notif_tx.clone();
        let target_id_bg = target_id.clone();
        let channel_bg = channel_name.clone();
        tokio::spawn(async move {
            // Start event collection FIRST (in parallel with send_prompt)
            let mut event_rx = client.subscribe_events();
            let events = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
            let events_clone = Arc::clone(&events);

            // Event collection task - runs in background
            let notif_tx_clone = notif_tx_bg.clone();
            let target_id_clone = target_id_bg.clone();
            let channel_clone = channel_bg.clone();
            let _event_collector = tokio::spawn(async move {
                let mut pending = String::new();
                let mut interval = 0u64;
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
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
                                        if r.len() > 100 {
                                            format!("✅ {}...", &r[..100])
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
                                crate::acp::client::SessionEvent::AgentThoughtChunk {
                                    content,
                                    ..
                                } => {
                                    format!("💭 {}", content)
                                }
                                _ => String::new(),
                            };

                            if !event_str.is_empty() {
                                events_clone.lock().await.push(event_str.clone());
                                if let Some(ref tx) = notif_tx_clone {
                                    pending.push_str(&event_str);
                                    pending.push('\n');
                                    interval += 1;
                                    if interval >= 3 || pending.len() > 400 {
                                        let _ = tx.send(crate::channel::OutboundMessage {
                                            target_id: target_id_clone.clone(),
                                            is_group: false,
                                            text: format!("🔄 Claude Code\n{}", pending.trim()),
                                            reply_to: None,
                                            images: vec![],
                                            channel: Some(channel_clone.clone()),
                                        });
                                        pending.clear();
                                        interval = 0;
                                    }
                                }
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

                    let events_text = events.lock().await.join("\n");
                    let collected = client.get_collected_content().await;
                    tracing::info!(
                        "tool_claudecode: events_text len={}, collected len={}",
                        events_text.len(),
                        collected.len()
                    );

                    let output = if !events_text.is_empty() {
                        events_text
                    } else if !collected.is_empty() {
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
                        "(无输出)".to_string()
                    };

                    let final_output = if output.len() > 4000 {
                        format!("{}...\n\n[已截断]", &output[..4000])
                    } else {
                        output
                    };

                    // Send notification to user
                    if let Some(ref tx) = notif_tx_bg {
                        let _ = tx.send(crate::channel::OutboundMessage {
                            target_id: target_id_bg.clone(),
                            is_group: false,
                            text: format!("✅ Claude Code 完成\n\n{}", final_output),
                            reply_to: None,
                            images: vec![],
                            channel: Some(channel_bg.clone()),
                        });
                    }
                }
                Err(e) => {
                    tracing::error!("tool_claudecode: send_prompt failed: {}", e);
                    if let Some(ref tx) = notif_tx_bg {
                        let _ = tx.send(crate::channel::OutboundMessage {
                            target_id: target_id_bg.clone(),
                            is_group: false,
                            text: format!("❌ Claude Code 错误\n\n{}", e),
                            reply_to: None,
                            images: vec![],
                            channel: Some(channel_bg.clone()),
                        });
                    }
                }
            }
            // DON'T await event_collector - it runs forever
        });

        Ok(serde_json::json!({
            "output": "Claude Code 任务已提交，完成后将推送结果。",
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
                 your general knowledge only."
                    .to_owned(),
            ),
            max_tokens: Some(500),
            temperature: None,
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
            pending_analysis: None,
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
                            pending_analysis: None,
                        });
                    }
                    // Has extractable text: return "analyzing..." immediately,
                    // attach pending analysis for the per-user worker to process.
                    return Ok(AgentReply {
                        text: crate::i18n::t("analyzing", i18n_lang),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        pending_analysis: Some(crate::agent::PendingAnalysis {
                            text: analysis_text,
                            session_key: session_key.to_owned(),
                            channel: channel.to_owned(),
                            peer_id: peer_id.to_owned(),
                        }),
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
                            pending_analysis: None,
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
                        pending_analysis: Some(crate::agent::PendingAnalysis {
                            text: analysis_text,
                            session_key: session_key.to_owned(),
                            channel: channel.to_owned(),
                            peer_id: peer_id.to_owned(),
                        }),
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
                        pending_analysis: None,
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
                    "__HELP__" => build_help_text_filtered(allowed),
                    "__VERSION__" => format!("rsclaw v{}", env!("RSCLAW_BUILD_VERSION")),
                    "__STATUS__" => {
                        let model = self.resolve_model_name();
                        let sessions = self.sessions.len();
                        let uptime = format_duration(self.started_at.elapsed());
                        format!(
                            "Gateway: running\nModel: {model}\nSessions: {sessions}\nUptime: {uptime}\nVersion: rsclaw v{}",
                            env!("RSCLAW_BUILD_VERSION")
                        )
                    }
                    "__HEALTH__" => {
                        let model = self.resolve_model_name();
                        let (prov_name, _) =
                            crate::provider::registry::ProviderRegistry::parse_model(&model);
                        let provider_ok = self.providers.get(prov_name).is_ok();
                        format!(
                            "Health check:\n  Provider ({}): {}\n  Store: ok\n  Agent: {}\n  Version: rsclaw v{}",
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
                        self.sessions.remove(session_key);
                        "Session cleared.".to_owned()
                    }
                    "__RESET__" => {
                        // Clear in-memory cache AND redb session data.
                        let key = self.resolve_session_key(session_key).to_owned();
                        self.sessions.remove(&key);
                        let _ = self.store.db.delete_session(&key);
                        "Session reset.".to_owned()
                    }
                    s if s.starts_with("__HISTORY__:") => {
                        let n: usize = s
                            .strip_prefix("__HISTORY__:")
                            .unwrap_or("20")
                            .parse()
                            .unwrap_or(20);
                        if let Some(msgs) = self.sessions.get(session_key) {
                            let start = msgs.len().saturating_sub(n);
                            let mut lines = Vec::new();
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
                            pending_analysis: None,
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
                        pending_analysis: None,
                    });
                }
                // Fall through to LLM for unhandled directives
            }
            crate::agent::preparse::PreParseResult::ToolCall { tool, args }
                if cmd_permitted(text) =>
            {
                // Group chat safety: block dangerous preparse commands (/run, /ls, /cat, etc.)
                let is_group = session_key.contains(":group:");
                if is_group && matches!(tool.as_str(), "exec" | "read" | "write") {
                    return Ok(AgentReply {
                        text: "[Blocked] Shell/file commands are not allowed in group chats for security.".to_owned(),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        pending_analysis: None,
                    });
                }
                info!(tool = %tool, "pre-parse: executing tool directly");
                let result = self
                    .dispatch_tool(
                        &RunContext {
                            agent_id: self.handle.id.clone(),
                            session_key: session_key.to_owned(),
                            channel: channel.to_owned(),
                            peer_id: peer_id.to_owned(),
                            loop_detector: crate::agent::loop_detection::LoopDetector::default(),
                            has_images: false,
                            user_msg_with_images: None,
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
                            pending_analysis: None,
                        });
                    }
                    Err(e) => {
                        return Ok(AgentReply {
                            text: format!("error: {e}"),
                            is_empty: false,
                            tool_calls: None,
                            images: vec![],
                            pending_analysis: None,
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
                        pending_analysis: None,
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
                        pending_analysis: None,
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
                    pending_analysis: None,
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
                pending_analysis: None,
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
                    pending_analysis: None,
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
                    pending_analysis: None,
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
                pending_analysis: None,
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
                const GROUP_BLOCKED_TOOLS: &[&str] = &["exec", "read", "write", "computer_use"];
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
                pending_analysis: None,
            });
        }

        let mut ctx = RunContext {
            agent_id: self.handle.id.clone(),
            session_key: session_key.to_owned(),
            channel: channel.to_owned(),
            peer_id: peer_id.to_owned(),
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
        };

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
            .unwrap_or(128_000) as usize;

        let mut tool_images: Vec<String> = Vec::new();

        loop {
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

            // Use built-in defaults if not configured
            let max_tokens = crate::provider::model_defaults::resolve_max_tokens(
                provider_name,
                model_id,
                configured_max_tokens,
            );

            // Log max_tokens resolution for debugging
            if configured_max_tokens.is_some() {
                info!(
                    session = %ctx.session_key,
                    model = %model,
                    configured = configured_max_tokens.unwrap(),
                    effective = max_tokens,
                    "LLM request max_tokens (from config)"
                );
            } else {
                info!(
                    session = %ctx.session_key,
                    model = %model,
                    effective = max_tokens,
                    "LLM request max_tokens (using builtin default)"
                );
            }

            let req = LlmRequest {
                model: model.to_owned(),
                messages,
                tools: tools.clone(),
                system: Some(effective_system.clone()),
                max_tokens: Some(max_tokens),
                temperature: None,
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
                        if reasoning_buf.len() <= 50 {
                            tracing::debug!(reasoning_len = reasoning_buf.len(), "agent_loop: got reasoning delta");
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
                                // if input is a string (partial args), try to merge.
                                if last.2 == serde_json::Value::Object(Default::default()) {
                                    last.2 = input;
                                } else if let (Some(existing), Some(new_str)) =
                                    (last.2.as_str(), input.as_str())
                                {
                                    // Merge the new delta and immediately try to repair.
                                    // This handles cases where the model sends malformed JSON
                                    // incrementally — repairing on each delta prevents garbage
                                    // accumulation and catches fixable issues early.
                                    let merged = format!("{existing}{new_str}");
                                    if let Some(repair) =
                                        crate::agent::tool_call_repair::try_extract_usable_args(
                                            &merged,
                                        )
                                    {
                                        tracing::debug!(
                                            repair_kind = ?repair.kind,
                                            "streaming tool call: real-time repair succeeded"
                                        );
                                        last.2 = repair.args;
                                    } else {
                                        last.2 = serde_json::Value::String(merged);
                                    }
                                }
                            }
                        }
                    }
                    StreamEvent::Done { .. } => {}
                    StreamEvent::Error(e) => {
                        return Err(anyhow!("LLM stream error: {e}"));
                    }
                }
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

            // Strip any residual <think>...</think> tags from accumulated text
            // (qwen3.5/QwQ may split tags across chunks).
            // Remember pre-strip length: if the model only produced thinking
            // content the user already saw it via streaming, so we treat the
            // empty post-strip result as a silent no-op rather than an error.
            let pre_strip_len = text_buf.trim().len();
            text_buf = crate::provider::openai::strip_think_tags_pub(&text_buf);

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

                    // First, try direct parse (preserves if valid)
                    let parsed = serde_json::from_str::<serde_json::Value>(&s);
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
                    pending_analysis: None,
                });
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
                    pending_analysis: None,
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
                        (format!("{{\"error\":\"{}\"}}", e), vec![])
                    }
                };

                tool_images.extend(result_images);

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
                    "exec" => limits.and_then(|l| l.exec).unwrap_or(3000),
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
            "agent" | "subagents" => return self.tool_agent_consolidated(args).await,
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
                    .tool_agent_consolidated(inject_action(args, "spawn"))
                    .await;
            }
            "agent_list" | "agents_list" => {
                return self
                    .tool_agent_consolidated(inject_action(args, "list"))
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
            "read" => return self.tool_read(args).await,
            "write" => return self.tool_write(args).await,
            "exec" => return self.tool_exec(args).await,
            "web_search" => return self.tool_web_search(args).await,
            "web_fetch" => return self.tool_web_fetch(args).await,
            "web_browser" | "browser" => return self.tool_web_browser(args).await,
            "computer_use" => return self.tool_computer_use(args).await,
            "image" => return self.tool_image(args).await,
            "pdf" => return self.tool_pdf(args).await,
            "tts" => return self.tool_tts(args).await,
            "message" => return self.tool_message(args).await,
            "cron" => return self.tool_cron(args).await,
            "gateway" => return self.tool_gateway(args).await,
            "pairing" => return self.tool_pairing(args).await,
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
            .map(
                |d| json!({"id": d.id, "scope": d.scope, "kind": d.kind, "text": d.display_text()}),
            )
            .collect();
        Ok(json!({"results": results}))
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
        // Also append to MEMORY.md for persistent system prompt injection
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
            .unwrap_or_else(|| std::path::PathBuf::from("."));

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
            let content = match pdf_extract::extract_text_from_mem(&pdf_bytes) {
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
            .unwrap_or_else(|| std::path::PathBuf::from("."));

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
        use crate::config::schema::CompactionMode;

        let Some(cfg) = self.config.agents.defaults.compaction.clone() else {
            return;
        };

        // Multi-condition compaction trigger: token threshold OR turn count OR time
        // elapsed. Whichever fires first.
        let token_threshold = cfg
            .reserve_tokens_floor
            .map(|t| t as usize)
            .unwrap_or(8_000);
        let max_turns: u32 = 20;
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
            "compaction check"
        );

        if !token_trigger && !turn_trigger && !time_trigger {
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

        // Helper: truncate to N chars (UTF-8 safe, early exit).
        fn truncate_chars(s: &str, max: usize) -> String {
            match s.char_indices().nth(max) {
                None => s.to_owned(),
                Some((byte_idx, _)) => {
                    let mut t = s[..byte_idx].to_owned();
                    t.push_str("...[truncated]");
                    t
                }
            }
        }

        // Helper: smart-truncate tool_call args — keep metadata, trim large content.
        //
        // Limits by field type:
        //   - "content", "old_string", "new_string": bulk code/text → 300 chars
        //   - "command": shell commands with URLs → 500 chars
        //   - all other fields: kept intact (paths, actions, schedules, etc.)
        //   - total serialized output capped at 2000 chars
        fn compact_tool_args(_tool_name: &str, input: &Value) -> String {
            const BULK_FIELDS: &[&str] = &["content", "old_string", "new_string"];
            const MAX_BULK_CHARS: usize = 300;
            const MAX_CMD_CHARS: usize = 500;
            const MAX_TOTAL_CHARS: usize = 2000;

            if let Some(obj) = input.as_object() {
                let needs_truncation = obj.iter().any(|(k, v)| {
                    let limit = if BULK_FIELDS.contains(&k.as_str()) {
                        MAX_BULK_CHARS
                    } else if k == "command" {
                        MAX_CMD_CHARS
                    } else {
                        return false; // non-bulk fields: never truncate
                    };
                    v.as_str()
                        .map(|s| s.char_indices().nth(limit).is_some())
                        .unwrap_or(false)
                });

                if needs_truncation {
                    let mut compact = serde_json::Map::new();
                    for (k, v) in obj {
                        let limit = if BULK_FIELDS.contains(&k.as_str()) {
                            Some(MAX_BULK_CHARS)
                        } else if k == "command" {
                            Some(MAX_CMD_CHARS)
                        } else {
                            None
                        };
                        if let (Some(limit), Some(s)) = (limit, v.as_str()) {
                            compact.insert(
                                k.clone(),
                                Value::String(truncate_chars(s, limit)),
                            );
                        } else {
                            compact.insert(k.clone(), v.clone());
                        }
                    }
                    let serialized = serde_json::to_string(&Value::Object(compact))
                        .unwrap_or_default();
                    return if serialized.char_indices().nth(MAX_TOTAL_CHARS).is_some() {
                        truncate_chars(&serialized, MAX_TOTAL_CHARS)
                    } else {
                        serialized
                    };
                }
            }

            // Small args or non-object: serialize as-is with safety net
            let full = serde_json::to_string(input).unwrap_or_default();
            if full.char_indices().nth(MAX_TOTAL_CHARS).is_some() {
                truncate_chars(&full, MAX_TOTAL_CHARS)
            } else {
                full
            }
        }

        // Helper: render messages as plain text transcript.
        //
        // Total output is capped (default 16K tokens) to avoid blowing
        // up the compact LLM's context window.
        //
        // Strategy: two-pass budget allocation.
        //   Pass 1: render all messages at full detail, measure sizes.
        //   Pass 2 (if over budget): render at reduced detail, allocating
        //           budget from newest to oldest. Recent messages get full
        //           detail first; older messages get progressively reduced
        //           detail until budget is exhausted. This ensures the most
        //           recent (and typically most relevant) context is preserved.
        let msgs_to_text = |msgs: &[Message]| -> String {
            // Default 16K tokens input for compact LLM. User can override via
            // compaction.maxTranscriptTokens in config.
            let max_total_tokens: usize = cfg.max_transcript_tokens.unwrap_or(16_000);

            // Render a single message. `detail` controls verbosity:
            //   2 = full (tool args + results)
            //   1 = medium (tool names + truncated args, no results)
            //   0 = minimal (tool names only, text truncated)
            let render_msg = |m: &Message, detail: u8| -> String {
                let role = format!("{:?}", m.role).to_lowercase();
                let body = match &m.content {
                    MessageContent::Text(t) => {
                        if detail == 0 { truncate_chars(t, 200) } else { t.clone() }
                    }
                    MessageContent::Parts(parts) => parts
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text } => Some(
                                if detail == 0 { truncate_chars(text, 200) } else { text.clone() }
                            ),
                            ContentPart::ToolUse { name, input, .. } => match detail {
                                2 => Some(format!("[tool_call: {name}({})]",
                                    compact_tool_args(name, input))),
                                1 => Some(format!("[tool_call: {name}({})]",
                                    truncate_chars(
                                        &serde_json::to_string(input).unwrap_or_default(), 100))),
                                _ => Some(format!("[tool_call: {name}]")),
                            },
                            ContentPart::ToolResult { tool_use_id: _, content, .. } => match detail {
                                2 => Some(format!("[tool_result: {}]",
                                    truncate_chars(content, 800))),
                                1 => Some(format!("[tool_result: {}]",
                                    truncate_chars(content, 150))),
                                _ => None, // drop results at minimal detail
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
            // Reuse Pass 1 token counts for detail=2 to avoid re-rendering.
            let n = msgs.len();
            let mut detail_levels = vec![0u8; n];
            let mut budget_used = 0usize;

            for i in (0..n).rev() {
                // Try full first (reuse cached token count), then medium/minimal.
                if budget_used + full_tokens[i] <= max_total_tokens {
                    detail_levels[i] = 2;
                    budget_used += full_tokens[i];
                } else {
                    // Try medium → minimal
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
                    break; // remaining older messages stay at detail=0 (default)
                }
            }

            // Final render in order. Reuse Pass 1 results for detail=2.
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

        // Persist compaction marker to transcript.
        self.append_transcript(
            session_key,
            "[auto-compaction triggered]",
            &format!("[summary: {summary}]"),
        )
        .await;
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
            max_tokens: Some(2048), // structured output needs more room
            temperature: None,
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

    async fn tool_exec(&self, args: Value) -> Result<Value> {
        tracing::debug!(?args, "tool_exec called");
        // Accept both "command" (rsclaw native) and "cmd"+"args" (preparse/openclaw format).
        let command = if let Some(cmd) = args["command"].as_str() {
            cmd.to_owned()
        } else if let Some(cmd) = args["cmd"].as_str() {
            // Reconstruct command string from cmd + args array.
            let cmd_args = args["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
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
            bail!("exec: `command` required");
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

        tracing::info!(cwd = %workspace.display(), command = %command, "exec: executing");

        // Timeout for exec commands (default 1800s = 30 min, matching openclaw).
        let timeout_secs = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.timeout_seconds)
            .unwrap_or(1800);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            tokio::process::Command::new(shell)
                .args(&shell_args)
                .arg(command)
                .current_dir(&workspace)
                .output()
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

        Ok(json!({
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
        }))
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
        let model = args["model"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_spawn: `model` required"))?
            .to_owned();
        let system = args["system"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_spawn: `system` required"))?
            .to_owned();

        use crate::config::schema::{AgentEntry, ModelConfig};

        let entry = AgentEntry {
            id: id.clone(),
            default: Some(false),
            workspace: Some(crate::config::loader::path_to_forward_slash(
                &dirs_next::home_dir()
                    .unwrap_or_default()
                    .join(format!(".rsclaw/workspace/{id}")),
            )),
            model: Some(ModelConfig {
                primary: Some(model),
                fallbacks: None,
                image: None,
                image_fallbacks: None,
                thinking: None,
                tools_enabled: None,
                toolset: None,
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

        // Write system prompt as SOUL.md in the new agent's workspace.
        let ws_path = dirs_next::home_dir()
            .unwrap_or_default()
            .join(format!(".rsclaw/workspace/{id}"));
        let _ = tokio::fs::create_dir_all(&ws_path).await;
        let soul_path = ws_path.join("SOUL.md");
        let _ = tokio::fs::write(&soul_path, format!("# Agent: {id}\n\n{system}\n")).await;

        Ok(json!({
            "spawned": id,
            "model": args["model"],
            "status": "ready"
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

        // Auto-detect provider: explicit arg > config default > keyed provider >
        // DuckDuckGo
        let chosen = if !provider.is_empty() {
            provider.to_owned()
        } else if let Some(default) = ws_cfg.and_then(|c| c.provider.as_deref()) {
            default.to_owned()
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
            .user_agent("Mozilla/5.0 (compatible; rsclaw/1.0)")
            .timeout(Duration::from_secs(15))
            .build()?;

        let results: Vec<Value> = match chosen.as_str() {
            "duckduckgo" => {
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
            // Free HTML scraping providers (no API key needed)
            "bing-free" => {
                let lang = self
                    .config
                    .raw
                    .gateway
                    .as_ref()
                    .and_then(|g| g.language.as_deref())
                    .unwrap_or("");
                let mkt = lang_to_bing_mkt(lang);
                let mkt_param = if mkt.is_empty() {
                    String::new()
                } else {
                    format!("&mkt={mkt}&setlang={}", &mkt[..2])
                };
                let url = format!(
                    "https://www.bing.com/search?q={}&count={limit}{mkt_param}",
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
            "baidu" => {
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
            "sogou" => {
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
            "360" => {
                let url = format!("https://www.so.com/s?q={}", urlencoding::encode(query));
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
                parse_360_results(&html, limit)
            }
            other => return Err(anyhow!("web_search: unknown provider `{other}`")),
        };

        // Fallback: if DDG returned empty (captcha), try bing-free
        if results.is_empty() && chosen == "duckduckgo" {
            tracing::warn!("web_search: DuckDuckGo returned 0 results, falling back to bing-free");
            let lang = self
                .config
                .raw
                .gateway
                .as_ref()
                .and_then(|g| g.language.as_deref())
                .unwrap_or("");
            let mkt = lang_to_bing_mkt(lang);
            let mkt_param = if mkt.is_empty() {
                String::new()
            } else {
                format!("&mkt={mkt}&setlang={}", &mkt[..2])
            };
            let url = format!(
                "https://www.bing.com/search?q={}&count={limit}{mkt_param}",
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
                return Ok(json!({ "results": fallback, "provider": "bing-free (fallback)" }));
            }
        }

        Ok(json!({ "results": results }))
    }

    async fn tool_web_fetch(&self, args: Value) -> Result<Value> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow!("web_fetch: `url` required"))?;

        let client = reqwest::Client::builder()
            .user_agent(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                 AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            )
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;

        let html = client.get(url).send().await?.text().await?;

        // Extract title.
        let title = extract_html_title(&html);

        // Strip HTML to plain text.
        let text = strip_html(&html);

        // Truncate to 50000 chars (char-safe for UTF-8).
        let truncated: String = if text.chars().count() > 50_000 {
            text.chars().take(50_000).collect()
        } else {
            text.clone()
        };

        Ok(json!({
            "url": url,
            "title": title,
            "text": truncated,
            "length": truncated.len()
        }))
    }

    async fn tool_web_browser(&self, args: Value) -> Result<Value> {
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

            // If no session, initialize one.
            if guard.is_none() {
                // Check Chrome availability
                let chrome_path = self.config.ext.tools.as_ref()
                    .and_then(|t| t.web_browser.as_ref())
                    .and_then(|b| b.chrome_path.clone())
                    .or_else(|| detect_chrome())
                    .ok_or_else(|| anyhow!(
                        "Chrome/Chromium not found. Install Chrome or set browser.chrome_path in config."
                    ))?;

                // Check memory before launching
                crate::browser::can_launch_chrome()?;

                let bs = crate::browser::BrowserSession::start(&chrome_path).await?;
                *guard = Some(bs);
            }
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

        match action {
            "screenshot" => {
                let tmp_path = std::env::temp_dir().join("rsclaw_screen.png");
                let tmp_path_str = tmp_path.to_string_lossy().to_string();

                let output = if is_macos {
                    tokio::process::Command::new("screencapture")
                        .args(["-x", &tmp_path_str])
                        .output()
                        .await
                } else if is_windows {
                    // Windows: use PowerShell with .NET
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
                    tokio::process::Command::new("powershell")
                        .args(["-NoProfile", "-Command", &script])
                        .output()
                        .await
                } else {
                    // Linux: try scrot first, fall back to import.
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

                let bytes = tokio::fs::read(&tmp_path)
                    .await
                    .map_err(|e| anyhow!("computer_use: failed to read screenshot: {e}"))?;
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let _ = tokio::fs::remove_file(&tmp_path).await;

                // Try to get image dimensions from the PNG header (width at bytes 16-19,
                // height at bytes 20-23, big-endian).
                let (width, height) = if bytes.len() >= 24 {
                    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
                    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
                    (w, h)
                } else {
                    (0, 0)
                };

                Ok(json!({
                    "action": "screenshot",
                    "image": format!("data:image/png;base64,{b64}"),
                    "width": width,
                    "height": height
                }))
            }
            "mouse_move" => {
                let x = args["x"].as_f64().unwrap_or(0.0) as i64;
                let y = args["y"].as_f64().unwrap_or(0.0) as i64;
                if is_macos {
                    run_subprocess("cliclick", &[&format!("m:{x},{y}")]).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", &x.to_string(), &y.to_string()])
                        .await?;
                }
                Ok(json!({"action": "mouse_move", "ok": true}))
            }
            "mouse_click" => {
                let x = args["x"].as_f64().unwrap_or(0.0) as i64;
                let y = args["y"].as_f64().unwrap_or(0.0) as i64;
                if is_macos {
                    run_subprocess("cliclick", &[&format!("c:{x},{y}")]).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", &x.to_string(), &y.to_string()])
                        .await?;
                    run_subprocess("xdotool", &["click", "1"]).await?;
                }
                Ok(json!({"action": "mouse_click", "ok": true}))
            }
            "type" => {
                let text = args["text"]
                    .as_str()
                    .ok_or_else(|| anyhow!("computer_use type: `text` required"))?;
                if is_macos {
                    // Use osascript for reliable typing.
                    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
                    run_subprocess(
                        "osascript",
                        &[
                            "-e",
                            &format!(
                                "tell application \"System Events\" to keystroke \"{escaped}\""
                            ),
                        ],
                    )
                    .await?;
                } else {
                    run_subprocess("xdotool", &["type", "--clearmodifiers", text]).await?;
                }
                Ok(json!({"action": "type", "ok": true}))
            }
            "key" => {
                let key = args["key"]
                    .as_str()
                    .ok_or_else(|| anyhow!("computer_use key: `key` required"))?;
                if is_macos {
                    run_subprocess("cliclick", &[&format!("kp:{key}")]).await?;
                } else {
                    run_subprocess("xdotool", &["key", key]).await?;
                }
                Ok(json!({"action": "key", "ok": true}))
            }
            other => Err(anyhow!(
                "computer_use: unsupported action `{other}` \
                 (supported: screenshot, mouse_move, mouse_click, type, key)"
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
        let text = match pdf_extract::extract_text_from_mem(&pdf_bytes) {
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
            let output = tokio::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
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

        // Cron config stored at openclaw-compatible path.
        // Respects OPENCLAW_STATE_DIR env var (same as openclaw).
        let cron_dir = if let Some(state_dir) = std::env::var_os("OPENCLAW_STATE_DIR") {
            tracing::debug!("tool_cron: OPENCLAW_STATE_DIR={}", state_dir.to_string_lossy());
            std::path::PathBuf::from(state_dir)
        } else {
            let home = dirs_next::home_dir().unwrap_or_default();
            tracing::debug!("tool_cron: OPENCLAW_STATE_DIR not set, home={}", home.display());
            home.join(".openclaw")
        };
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

    async fn tool_agent_consolidated(&self, args: Value) -> Result<Value> {
        let action = args["action"].as_str().unwrap_or("list");
        match action {
            "spawn" => self.tool_agent_spawn(args).await,
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
            _ => bail!("agent: unknown action '{action}' (spawn, list, kill)"),
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
        match pdf_extract::extract_text_from_mem(bytes) {
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

fn parse_360_results(html: &str, limit: usize) -> Vec<Value> {
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
            "No results found.".to_owned()
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

/// Detect Chrome/Chromium binary path.
fn detect_chrome() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let app_path = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
        if std::path::Path::new(app_path).exists() {
            return Some(app_path.to_owned());
        }
    }

    #[cfg(target_os = "windows")]
    {
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

// ---------------------------------------------------------------------------
// Help text builder
// ---------------------------------------------------------------------------

/// Compute the set of allowed tool names based on toolset level + custom tools.
/// Returns None for "full" (no filtering), Some(set) for others.
fn toolset_allowed_names(
    toolset: &str,
    custom_tools: Option<&Vec<String>>,
) -> Option<std::collections::HashSet<String>> {
    const MINIMAL: &[&str] = &["exec", "read", "write", "web_search", "web_fetch", "memory"];
    const STANDARD: &[&str] = &[
        "exec",
        "read",
        "write",
        "web_search",
        "web_fetch",
        "memory",
        "web_browser",
        "image",
        "channel",
        "cron",
    ];

    let base: Option<&[&str]> = match toolset {
        "minimal" => Some(MINIMAL),
        "standard" => Some(STANDARD),
        "full" => None,
        _ => Some(STANDARD), // unknown -> standard
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

fn build_help_text_filtered(allowed: &str) -> String {
    let full = allowed == "*";
    let has = |cmd: &str| -> bool {
        if full {
            return true;
        }
        READONLY_COMMANDS.iter().any(|c| *c == cmd) || allowed.split('|').any(|a| a.trim() == cmd)
    };

    let mut help = String::from("Available commands:\n\n");

    // Shell & Files -- require /run, /ls etc.
    if has("/run") || has("/find") || has("/grep") {
        help.push_str("Shell:\n");
        if has("/run") {
            help.push_str("  /run <cmd>        Execute a shell command\n");
            help.push_str("  $ <cmd>           Execute a shell command (shortcut)\n");
        }
        if has("/find") {
            help.push_str("  /find <pattern>   Find files by name\n");
        }
        if has("/grep") {
            help.push_str("  /grep <pattern>   Search file contents\n");
        }
        help.push('\n');
    }

    if has("/read") || has("/write") || has("/ls") {
        help.push_str("Files:\n");
        if has("/read") {
            help.push_str("  /read <path>      Read a file\n");
        }
        if has("/write") {
            help.push_str("  /write <path> <content>  Write to a file\n");
        }
        if has("/ls") {
            help.push_str("  /ls [path]        List directory\n");
        }
        help.push('\n');
    }

    if has("/search") || has("/fetch") || has("/screenshot") || has("/ss") {
        help.push_str("Search & Web:\n");
        if has("/search") {
            help.push_str("  /search <query>   Search the web\n");
        }
        if has("/fetch") {
            help.push_str("  /fetch <url>      Fetch a web page\n");
        }
        if has("/screenshot") {
            help.push_str("  /screenshot <url> Screenshot a web page\n");
        }
        if has("/ss") {
            help.push_str("  /ss               Screenshot desktop\n");
        }
        help.push('\n');
    }

    if has("/remember") || has("/recall") {
        help.push_str("Memory:\n");
        if has("/remember") {
            help.push_str("  /remember <text>  Save to memory\n");
        }
        if has("/recall") {
            help.push_str("  /recall <query>   Search memory\n");
        }
        help.push('\n');
    }

    // Always show /ctx and /btw (read-only commands)
    help.push_str("Background Context:\n");
    help.push_str("  /ctx <text>              Add persistent context\n");
    help.push_str("  /ctx --ttl <N> <text>    Add context (expires in N turns)\n");
    if full {
        help.push_str("  /ctx --global <text>     Add global context (all sessions)\n");
    }
    help.push_str("  /ctx --list              List active context entries\n");
    help.push_str("  /ctx --remove <id>       Remove entry by id\n");
    help.push_str("  /ctx --clear             Clear all context for this session\n");
    help.push('\n');

    help.push_str("Side Query:\n");
    help.push_str("  /btw <question>          Quick query (no tools, ephemeral)\n");
    help.push('\n');

    if full {
        help.push_str("Tools (consolidated):\n");
        help.push_str("  memory   search/get/put/delete long-term memory\n");
        help.push_str("  session  send/list/history/status for sessions\n");
        help.push_str("  agent    spawn/list/kill sub-agents\n");
        help.push_str("  channel  send/reply/pin/delete across channels\n");
        help.push('\n');
    }

    help.push_str("System:\n");
    help.push_str("  /status           Gateway status\n");
    help.push_str("  /version          Show version\n");
    help.push_str("  /models           List models\n");
    if has("/model") {
        help.push_str("  /model <name>     Switch model\n");
    }
    help.push_str("  /uptime           Show uptime\n");
    help.push('\n');

    help.push_str("Session:\n");
    help.push_str("  /clear            Clear session\n");
    if has("/reset") {
        help.push_str("  /reset            Reset session\n");
    }
    help.push_str("  /history [n]      Show history\n");
    if has("/sessions") {
        help.push_str("  /sessions         List sessions\n");
    }
    help.push('\n');

    help.push_str("Cron:\n");
    help.push_str("  /cron list        List cron jobs\n");
    help.push('\n');

    if has("/send") {
        help.push_str("Messaging:\n");
        help.push_str("  /send <target> <msg>  Send a message\n");
        help.push('\n');
    }

    if has("/skill") {
        help.push_str("Skill:\n");
        help.push_str("  /skill install <name>\n");
        help.push_str("  /skill list\n");
        help.push_str("  /skill search <query>\n");
        help.push('\n');
    }

    if full {
        help.push_str("Upload & Limits:\n");
        help.push_str("  /get_upload_size           Show upload size limit\n");
        help.push_str("  /set_upload_size <MB>      Set size limit (runtime)\n");
        help.push_str("  /get_upload_chars          Show text char limit\n");
        help.push_str("  /set_upload_chars <N>      Set char limit (runtime)\n");
        help.push_str("  /config_upload_size <MB>   Set size limit (persistent)\n");
        help.push_str("  /config_upload_chars <N>   Set char limit (persistent)\n");
        help.push('\n');
    }

    help.push_str("Type any message without / to chat with the AI agent.");

    help
}

// ---------------------------------------------------------------------------
// System prompt builder
// ---------------------------------------------------------------------------

fn build_system_prompt(
    ws_ctx: &WorkspaceContext,
    skills: &SkillRegistry,
    config: &crate::config::schema::Config,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Current date/time so the model knows "today", "last Friday", etc.
    let now = chrono::Local::now();
    // Calculate useful reference dates
    use chrono::Datelike;
    let weekday = now.date_naive().weekday().num_days_from_monday(); // 0=Mon
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
    // Language preference from config (gateway.language)
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

    // Tool usage guidance
    parts.push(
        "## Tool Usage Guidelines\n\
         - For code generation: write complete files, one module at a time.\n\
         - Use edit tool for small changes to existing files.\n\
         - For cron jobs: use the `cron` tool (action=list/add/remove). The `cron` tool is a first-class tool — always use it instead of trying to invoke a `cron` shell command."
            .to_string(),
    );

    // Workspace files segment.
    let ws_segment = ws_ctx.to_prompt_segment();
    if !ws_segment.is_empty() {
        parts.push(ws_segment);
    }

    // Available skills XML (AGENTS.md §20, item 3).
    if !skills.is_empty() {
        let skill_xml: String = skills
            .all()
            .map(|s| {
                format!(
                    "  <skill name=\"{}\">{}</skill>",
                    s.name,
                    s.description.as_deref().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!(
            "<available_skills>\n{skill_xml}\n</available_skills>"
        ));
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
                "kind":   {"type": "string", "description": "Document kind: note, fact, summary (for put)"},
                "top_k":  {"type": "integer", "description": "Max results (for search, default 5)"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "read".to_owned(),
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
        name: "write".to_owned(),
        description: "Write content to a file in the agent workspace. Creates parent directories as needed. \
            Both 'path' and 'content' parameters are required. \
            Path is relative to workspace root (e.g., 'output.py', 'src/main.rs').".to_owned(),
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
        name: "exec".to_owned(),
        description: if cfg!(target_os = "windows") {
            "Run a shell command on Windows. Shell: PowerShell. Use PowerShell syntax: Get-ChildItem, Get-Content, Get-Date, Where-Object, Select-Object. Example: Get-Date; Get-ChildItem | Select-Object -Last 5".to_owned()
        } else if cfg!(target_os = "macos") {
            "Run a shell command on macOS. Shell: bash/zsh. Unix commands available (ls, cat, grep, tail).".to_owned()
        } else {
            "Run a shell command on Linux. Shell: bash/sh. Unix commands available (ls, cat, grep, tail).".to_owned()
        },
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"}
            },
            "required": ["command"]
        }),
    });
    tools.push(ToolDef {
        name: "agent".to_owned(),
        description: "Manage agents. Actions: spawn (create new sub-agent), list (all registered agents), kill (stop an agent).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["spawn", "list", "kill"], "description": "Action to perform"},
                "id":     {"type": "string", "description": "Agent ID (for spawn/kill)"},
                "model":  {"type": "string", "description": "Model string (for spawn)"},
                "system": {"type": "string", "description": "System prompt (for spawn)"}
            },
            "required": ["action"]
        }),
    });

    // Web tools.
    tools.push(ToolDef {
        name: "web_search".to_owned(),
        description: "Search the web using a configurable search engine. Returns titles, URLs, and snippets.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query":    {"type": "string", "description": "Search query"},
                "provider": {"type": "string", "description": "Search provider: duckduckgo, google, bing, brave. Leave empty to use the configured default."},
                "limit":    {"type": "integer", "description": "Max results to return (default 5)"}
            },
            "required": ["query"]
        }),
    });
    tools.push(ToolDef {
        name: "web_fetch".to_owned(),
        description: "Download a web page and extract its text content. Strips HTML tags, scripts, and styles. Truncates to 50000 chars.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "url":      {"type": "string", "description": "URL to fetch"},
                "selector": {"type": "string", "description": "CSS-like selector hint for content extraction"}
            },
            "required": ["url"]
        }),
    });
    tools.push(ToolDef {
        name: "web_browser".to_owned(),
        description: "Control a web browser via CDP. Actions: open, snapshot, click, fill, type, select, check, uncheck, scroll, screenshot, pdf, back, forward, reload, get_text, get_url, get_title, wait, evaluate, cookies".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action":    {"type": "string", "description": "Action to perform"},
                "url":       {"type": "string", "description": "URL for open action"},
                "ref":       {"type": "string", "description": "Element reference like @e3 from snapshot"},
                "text":      {"type": "string", "description": "Text for fill/type actions"},
                "value":     {"type": "string", "description": "Value for select action"},
                "direction": {"type": "string", "description": "up/down for scroll"},
                "js":        {"type": "string", "description": "JavaScript for evaluate action"},
                "target":    {"type": "string", "description": "element/text/url for wait action"},
                "timeout":   {"type": "number", "description": "Timeout in seconds (default 15)"}
            },
            "required": ["action"]
        }),
    });
    tools.push(ToolDef {
        name: "computer_use".to_owned(),
        description: "Control the computer: take screenshots, move/click mouse, type text, press keys. Works on macOS (screencapture, cliclick, osascript) and Linux (scrot, xdotool).".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "description": "Action: screenshot, mouse_move, mouse_click, type, key"},
                "x":      {"type": "number", "description": "X coordinate for mouse actions"},
                "y":      {"type": "number", "description": "Y coordinate for mouse actions"},
                "text":   {"type": "string", "description": "Text for type action"},
                "key":    {"type": "string", "description": "Key name for key action (e.g. enter, tab, escape)"}
            },
            "required": ["action"]
        }),
    });

    // --- New openclaw-compatible tools ---

    tools.push(ToolDef {
        name: "image".to_owned(),
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
        name: "tts".to_owned(),
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
        name: "message".to_owned(),
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
        tracing::debug!(
            context_tokens,
            history_budget,
            total_tokens,
            removed = remove_count,
            "context budget trim: removed {remove_count} oldest messages"
        );
        messages.drain(..remove_count);
    }
}

// ---------------------------------------------------------------------------
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
            "memory", "session", "agent", "channel", "read", "write", "exec",
        ] {
            assert!(
                names.contains(expected),
                "expected built-in tool `{expected}` in tool list, got: {names:?}"
            );
        }
    }
}
