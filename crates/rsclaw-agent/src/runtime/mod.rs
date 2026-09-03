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
//!   9. Auto-Recall (inject relevant memories) + Auto-Capture (extract durable
//!      entities — NOT raw user messages; see
//!      docs/memory-extraction-redesign.md)

mod agent_loop;
mod dispatch;
mod run_turn;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
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
    /// Tools currently running this turn (parallel dispatch). Each entry
    /// is (tool_name, started_at). Populated at dispatch_tool entry,
    /// drained at exit. /status renders one line per entry so the user
    /// can see "search_file running for 8min" instead of a silent hang.
    pub in_flight_tools: Vec<(String, std::time::Instant)>,
}

use rsclaw_config::{live_config::LiveConfig, runtime::RuntimeConfig};
use rsclaw_events::AgentEvent;
use rsclaw_plugin::PluginRegistry;
use rsclaw_provider::{
    AgentEndpoint, ContentPart, LlmRequest, Message, MessageContent, RecallBundle, RetryConfig,
    Role, StreamEvent, ToolDef, failover::FailoverManager, registry::ProviderRegistry,
};
use rsclaw_skill::SkillRegistry;
use rsclaw_store::Store;

pub use super::context_mgr::estimate_tokens;
use super::{
    context_mgr::{
        apply_context_budget_trim, apply_context_pruning, build_clear_summary, msg_tokens,
    },
    loop_detection::LoopDetector,
    memory::MemoryStore,
    prompt_builder::{
        READONLY_COMMANDS, build_help_text_filtered, build_minimal_system_prompt,
        build_system_prompt, format_duration,
    },
    registry::{AgentHandle, AgentMessage, AgentRegistry, AgentReply},
    security::check_read_safety,
    tool_call_repair::repair_tool_result_pairing,
    tools_builder::{build_tool_list, toolset_allowed_names},
    workspace::{DEFAULT_MAX_CHARS_PER_FILE, DEFAULT_TOTAL_MAX_CHARS, SessionType},
};

/// Agent-level timeout for a single turn (seconds).
/// Reduced from OpenClaw's 48h default to 30min for better UX.
/// Can be overridden via `agents.defaults.timeout_seconds`.
pub(crate) const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
/// Idle watchdog for the LLM response stream: if no event arrives within this
/// window the connection is treated as stalled and the turn fails. This is an
/// *inactivity* limit, not a total-duration cap — a long but actively-streaming
/// turn never trips it. It exists to recover the single-threaded worker queue
/// from "connected, 200 OK, then silent forever" hangs without waiting for the
/// 30-minute turn timeout. Sized generously above worst-case time-to-first-
/// token on a loaded GGUF fleet (large-context prefill) to avoid false kills.
const STREAM_IDLE_TIMEOUT_SECS: u64 = 120;
/// Max consecutive tool parse errors before aborting the turn.
/// Prevents infinite retry loops when model output gets corrupted.
const MAX_PARSE_ERRORS: usize = 10;
/// Per-tool wall-clock ceiling inside a single turn's parallel dispatch.
/// One wedged sub-agent / hung HTTP call no longer holds the whole batch
/// hostage — it is reported as a tool error after this many seconds.
/// Generous because legitimate exec/image-gen tools may run minutes.
const TOOL_DISPATCH_TIMEOUT_SECS: u64 = 600;
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

/// Per-session plugin activation override. Stored on `AgentHandle` so the
/// `/plugin` slash command (which only has `&AgentHandle`) can mutate it.
/// Resolved at system-prompt build time to render an "## Active Plugin
/// Tools" block (full input_schema) into user_system. The block lives in
/// the per-session segment of the prompt, so it doesn't break the shared
/// prefix KV cache hash.
///
/// **Defaults (no entry in `plugin_overrides`) = today's behavior:** no
/// active block emitted, model must use `plugin_search` + `plugin_invoke`.
/// Setting one of the variants below opts a single (session, plugin) into
/// an upgraded exposure.
#[derive(Debug, Clone, Default)]
pub struct PluginOverride {
    /// Plugin is hidden entirely in this session (catalog + tools).
    pub disabled: bool,
    /// Inject every tool this plugin exposes — REPLACES the
    /// headline-based default. Capped by `user_tools_cap`.
    pub inject_all: bool,
    /// REPLACE the headline-based default with this exact list. Empty
    /// means "use the default" (headlines + agent-level pins).
    pub inject: Vec<String>,
    /// ADD these tool names on top of whatever the base set resolves
    /// to (headline default, `inject`, or `inject_all`). Slash command
    /// `/plugin pin <plugin>__<tool>` writes here.
    pub pin: Vec<String>,
    /// REMOVE these tool names from the resolved base set. Wins over
    /// `pin` if both reference the same name (unpin is final).
    /// `/plugin unpin <plugin>__<tool>` writes here.
    pub unpin: Vec<String>,
}

/// Result of resolving a plugin override to its inject set. The `All`
/// variant defers expansion to the caller (which has access to the live
/// plugin tool list) instead of cloning the full name list through the
/// override store on every set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginInjectResolution {
    /// Nothing to inject (default state, or plugin disabled).
    None,
    /// Inject every tool the plugin currently exposes.
    All,
    /// Inject exactly these tool names.
    Names(Vec<String>),
}

/// Default cap on plugin tools exposed in `dynamic_prefix.user_tools`
/// per turn when `model.user_tools_cap` is unset. Sized to fit ~10
/// headlines × 3 active plugins without overflowing a 64k-context
/// small model's prompt budget; larger models override upward.
pub(crate) const DEFAULT_USER_TOOLS_CAP: usize = 30;

/// Wire name separator between plugin namespace and tool name. Chosen
/// as double-underscore because OpenAI's tool-name regex
/// `^[a-zA-Z0-9_-]+$` rejects the dot (`.`) the legacy code used,
/// while every modern provider including rsclaw-llm accepts `_`.
/// Legacy `<plugin>.<tool>` is still accepted on inbound tool calls
/// so old transcripts replay.
pub(crate) const PLUGIN_TOOL_SEP: &str = "__";

fn is_stock_tool_name(name: &str) -> bool {
    matches!(
        name,
        "stock_quote"
            | "stock_select"
            | "stock_lhb"
            | "stock_debate"
            | "stock_iwencai"
            | "stock_kline"
            | "stock_snapshot"
            | "stock_ask"
            | "stock_query"
            | "stock_chart"
            | "stock_watchlist"
            | "stock_holdings"
    )
}

/// A plugin tool that's been selected for inclusion in
/// `dynamic_prefix.user_tools`. Owns its data so the caller doesn't
/// need to hold a borrow on the plugin registry while building the
/// final ToolDef array.
#[derive(Debug, Clone)]
pub(crate) struct PluginUserToolSelection {
    pub plugin_name: String,
    pub tool_name: String,
    pub description: String,
    pub input_schema: Value,
    /// v2 toolGroups: feature group this tool belongs to, if declared.
    /// Populated during selection; retained for grouping/telemetry even
    /// though no reader consumes it yet.
    #[allow(dead_code)]
    pub group: Option<String>,
}

impl PluginUserToolSelection {
    /// Render the wire-facing tool name as `<plugin><sep><tool>`.
    pub(crate) fn wire_name(&self) -> String {
        format!("{}{}{}", self.plugin_name, PLUGIN_TOOL_SEP, self.tool_name)
    }
}

/// Parse a qualified tool reference. Accepts the new `<plugin>__<tool>`
/// canonical form, the legacy `<plugin>.<tool>` form (still emitted by
/// old `model.plugin_tools` configs), and `<plugin>/<tool>` (operator
/// muscle memory from skill paths). Returns `None` when the entry has
/// no recognized separator — caller logs and skips.
pub fn parse_qualified_tool(entry: &str) -> Option<(String, String)> {
    let entry = entry.trim();
    if let Some((p, t)) = entry.split_once(PLUGIN_TOOL_SEP) {
        return Some((p.trim().to_owned(), t.trim().to_owned()));
    }
    if let Some((p, t)) = entry.split_once('.') {
        return Some((p.trim().to_owned(), t.trim().to_owned()));
    }
    if let Some((p, t)) = entry.split_once('/') {
        return Some((p.trim().to_owned(), t.trim().to_owned()));
    }
    None
}

/// Group a flat list of qualified tool references (`<plugin>__<tool>`,
/// or legacy `<plugin>.<tool>`) into a per-plugin lookup. Used by
/// `select_user_tools_pure` so pin/unpin checks are O(active tools)
/// instead of O(config_entries × active_tools). Entries without a
/// recognized separator are silently dropped — the caller already
/// logged on a prior pass that produced this list.
pub(crate) fn bucket_qualified_names(
    entries: &[String],
) -> std::collections::HashMap<String, std::collections::HashSet<String>> {
    let mut out: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for entry in entries {
        if let Some((plugin, tool)) = parse_qualified_tool(entry) {
            out.entry(plugin).or_default().insert(tool);
        }
    }
    out
}

#[derive(Debug, Clone)]
pub(crate) struct PluginToolInfo {
    pub(crate) plugin: String,
    pub(crate) runtime: &'static str,
    pub(crate) tool: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

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
    /// Video upload awaiting the 4-option processing menu (extract audio /
    /// analyze frames / both / delete). `PendingFile.path` points at the
    /// SAVED uploads copy (not a temp duplicate) so ffmpeg can read it and
    /// "delete" removes the real file.
    VideoMenu,
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

/// Internal sessions are ephemeral: their transcripts are not loaded from
/// or persisted to redb, and stale entries are purged on boot. Used to
/// avoid "HEARTBEAT_OK" replies and per-job cron output accumulating in
/// session history.
///
/// Note this does NOT govern prompt/tool minimization — see
/// `is_minimal_context_session` for that. Cron jobs are ephemeral but
/// run as user-initiated turns with the full agent prompt and tool set.
fn is_internal_session(session_key: &str) -> bool {
    session_key.starts_with("heartbeat:")
        || session_key.starts_with("cron:")
        || session_key.starts_with("system:")
}

/// Sessions that should run with a minimal system prompt and only the
/// `memory` tool. These are auto-tick style turns (heartbeat ping, system
/// maintenance) where the LLM is expected to reply briefly or do memory
/// upkeep — not to execute user actions.
///
/// Cron is intentionally excluded: cron-fired `agentTurn` payloads carry
/// real user instructions (e.g. "执行全屏截图发送给用户") that need the
/// full system prompt and tool set, even though the session itself is
/// ephemeral.
fn is_minimal_context_session(session_key: &str) -> bool {
    session_key.starts_with("heartbeat:") || session_key.starts_with("system:")
}

/// Convert an image reference (file path or data URL) into a `data:` URL
/// suitable for non-WS channels.
///
/// `tool_images` in the agent loop may hold either:
///   - a `data:image/...;base64,...` URL (image-gen tools, inline uploads),
///   - an `http(s)://...` URL (remote images already usable by channels), or
///   - a local file path (computer_use screenshots, saved to disk to avoid
///     shipping base64 through the WS event bus).
///
/// Returns `None` if the file cannot be read — the image is simply dropped
/// rather than breaking the whole reply.
fn image_ref_to_data_url(image_ref: String) -> Option<String> {
    if image_ref.starts_with("data:")
        || image_ref.starts_with("http://")
        || image_ref.starts_with("https://")
    {
        return Some(image_ref);
    }
    match std::fs::read(&image_ref) {
        Ok(bytes) => {
            use base64::Engine as _;
            let ext = std::path::Path::new(&image_ref)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase());
            let mime = match ext.as_deref() {
                Some("jpg") | Some("jpeg") => "image/jpeg",
                Some("webp") => "image/webp",
                Some("gif") => "image/gif",
                Some("bmp") => "image/bmp",
                _ => "image/png",
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Some(format!("data:{mime};base64,{b64}"))
        }
        Err(e) => {
            tracing::warn!(
                path = %image_ref,
                error = %e,
                "image_ref_to_data_url: read failed, dropping image"
            );
            None
        }
    }
}

/// Check if the current model supports vision (image input).
/// Detect a natural-language intent to switch voice/text reply mode.
///
/// Returns `Some(true)` to switch to voice, `Some(false)` to switch to
/// text, `None` if the user said nothing about reply mode. Used so the
/// user doesn't have to remember the explicit `/voice` · `/text` slash
/// commands — typing "用文字回复" or "no voice please" works too.
///
/// Implementation is intentionally a tiny keyword list, not a regex
/// engine. False positives on weird phrasings are acceptable; the user
/// can always issue `/voice` or `/text` explicitly to override.
fn parse_voice_mode_intent(text: &str) -> Option<bool> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();

    const OFF_ZH: &[&str] = &[
        "\u{7528}\u{6587}\u{5B57}",         // 用文字
        "\u{6539}\u{6210}\u{6587}\u{5B57}", // 改成文字
        "\u{56DE}\u{590D}\u{6587}\u{5B57}", // 回复文字
        "\u{6587}\u{5B57}\u{56DE}\u{590D}", // 文字回复
        "\u{4E0D}\u{8981}\u{8BED}\u{97F3}", // 不要语音
        "\u{4E0D}\u{7528}\u{8BED}\u{97F3}", // 不用语音
        "\u{522B}\u{8BED}\u{97F3}",         // 别语音
        "\u{505C}\u{6B62}\u{8BED}\u{97F3}", // 停止语音
        "\u{5173}\u{6389}\u{8BED}\u{97F3}", // 关掉语音
        "\u{5173}\u{95ED}\u{8BED}\u{97F3}", // 关闭语音
    ];
    const OFF_EN: &[&str] = &[
        "text only",
        "no voice",
        "stop voice",
        "reply in text",
        "respond in text",
        "text reply",
        "switch to text",
    ];
    const ON_ZH: &[&str] = &[
        "\u{7528}\u{8BED}\u{97F3}",         // 用语音
        "\u{8BED}\u{97F3}\u{56DE}\u{590D}", // 语音回复
        "\u{6539}\u{6210}\u{8BED}\u{97F3}", // 改成语音
        "\u{5207}\u{8BED}\u{97F3}",         // 切语音
    ];
    const ON_EN: &[&str] = &[
        "reply in voice",
        "voice reply",
        "use voice",
        "switch to voice",
    ];

    let says_off =
        OFF_ZH.iter().any(|p| trimmed.contains(p)) || OFF_EN.iter().any(|p| lower.contains(p));
    let says_on =
        ON_ZH.iter().any(|p| trimmed.contains(p)) || ON_EN.iter().any(|p| lower.contains(p));
    match (says_off, says_on) {
        (true, false) => Some(false),
        (false, true) => Some(true),
        _ => None, // ambiguous (both / neither) — leave mode unchanged
    }
}

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
    /// Inbound account key (e.g. feishu app account name). Threaded to
    /// outbound `OutboundMessage.account` so notifications route via
    /// `<channel>/<account>` and not bare `<channel>` — fixes Feishu
    /// 99992361 "open_id cross app" on multi-app deployments where
    /// the first-registered app would otherwise swallow every send.
    pub account: Option<String>,
    /// Background exec pool for polling task results.
    pub exec_pool: Arc<super::exec_pool::ExecPool>,
    pub loop_detector: LoopDetector,
    /// Whether the current turn includes images.
    pub has_images: bool,
    /// The full user message with image data (for LLM, not persisted).
    pub user_msg_with_images: Option<Message>,
    /// Count of consecutive tool parse errors in this turn.
    pub parse_error_count: usize,
    /// Memory doc IDs recalled during this turn (auto-recall +
    /// tool_memory_search).
    pub recalled_memory_ids: std::collections::HashSet<String>,
    /// Hidden committed recall bundle for the first user-delta LLM call.
    pub auto_recall: Option<RecallBundle>,
    /// Whether a loop-detection warning was triggered during this turn.
    pub loop_warning_triggered: bool,
    /// Factual trace of the looping tool call (tool name + args + warning) set
    /// when a loop is detected. Drives failure-lesson extraction at end of
    /// turn.
    pub loop_failure: Option<String>,
    /// Per-turn difficulty counters for workflow crystallization.
    pub turn_metrics: super::turn_metrics::TurnMetrics,
    /// The original user request text for this turn — saved on RunContext
    /// so the workflow distiller has the verbatim ask without re-walking
    /// session history.
    pub user_text: String,
    /// Optional lossless trace for SFT data export. Populated only when
    /// `RSCLAW_CAPTURE_TRACES=1`; flushed to the JSONL path in
    /// `RSCLAW_TRACES_PATH` on normal turn completion.
    pub full_trace: Option<super::trace_capture::FullTrace>,
    /// Per-turn observability/control wires for A2A callers
    /// (cancel_token, event_tx, input_request_tx, task/context ids).
    /// Default-constructed for non-A2A turns; `agent_loop` polls
    /// `is_cancelled()` between iterations and at tool-dispatch
    /// boundaries, and calls `emit_working()` before each tool.
    pub turn_ctx: super::registry::TurnContext,
}

fn init_full_trace(user_text: &str) -> Option<super::trace_capture::FullTrace> {
    if std::env::var("RSCLAW_CAPTURE_TRACES").ok().as_deref() != Some("1") {
        return None;
    }
    let trace_id = format!(
        "trace-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let mut t =
        super::trace_capture::FullTrace::new(trace_id, String::new(), String::new(), json!([]));
    t.push_user(user_text);
    Some(t)
}

/// Lazily-spawned single-writer task for RSCLAW_TRACES_PATH JSONL output.
///
/// Two correctness requirements drove the refactor away from a direct
/// sync write inside `maybe_emit_trace`:
///   1. Hot path: the previous implementation ran `std::fs::File` +
///      `BufWriter::write_all` synchronously on the tokio worker thread that
///      just finished an agent turn. Under disk pressure / slow network FS that
///      thread blocked, stalling other agents sharing the runtime.
///   2. Concurrency safety: two agent loops emitting traces for the same path
///      interleaved bytes (POSIX `O_APPEND` is only atomic for writes ≤
///      PIPE_BUF). A realistic trace is tens of KB and torn lines fail to
///      parse, silently dropping training samples.
///
/// The dedicated writer task owns the file handle, receives owned
/// FullTrace values over an unbounded mpsc, and serializes async writes
/// via tokio::io::BufWriter. Emit becomes O(1) channel-send on the hot
/// path; the writer drains in the background.
static TRACE_TX: std::sync::OnceLock<
    tokio::sync::mpsc::UnboundedSender<super::trace_capture::FullTrace>,
> = std::sync::OnceLock::new();

fn spawn_trace_writer(
    path: std::path::PathBuf,
) -> tokio::sync::mpsc::UnboundedSender<super::trace_capture::FullTrace> {
    use tokio::io::AsyncWriteExt;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<super::trace_capture::FullTrace>();
    tokio::spawn(async move {
        let file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                warn!(?path, "trace writer: open failed: {e:#}");
                return;
            }
        };
        let mut w = tokio::io::BufWriter::new(file);
        while let Some(trace) = rx.recv().await {
            let entry = match serde_json::to_string(&super::sft_exporter::trace_to_sharegpt(&trace))
            {
                Ok(s) => s,
                Err(e) => {
                    warn!(trace_id = %trace.trace_id, "trace writer: serialize failed: {e:#}");
                    continue;
                }
            };
            if w.write_all(entry.as_bytes()).await.is_err()
                || w.write_all(b"\n").await.is_err()
                || w.flush().await.is_err()
            {
                warn!(?path, "trace writer: write failed; dropping trace");
                continue;
            }
        }
    });
    tx
}

fn maybe_emit_trace(trace: super::trace_capture::FullTrace) {
    let Ok(path_str) = std::env::var("RSCLAW_TRACES_PATH") else {
        return;
    };
    let tx = TRACE_TX.get_or_init(|| spawn_trace_writer(std::path::PathBuf::from(&path_str)));
    if tx.send(trace).is_err() {
        warn!("trace dropped: writer task is dead");
    }
}

// ---------------------------------------------------------------------------
// AgentRuntime
// ---------------------------------------------------------------------------

pub struct AgentRuntime {
    pub handle: Arc<AgentHandle>,
    pub config: Arc<RuntimeConfig>,
    /// Live, hot-mutable config slices (temperature, etc.). Read at request
    /// time so users can tune values without restarting the gateway.
    pub live: Arc<LiveConfig>,
    /// All registered providers — used by the failover manager.
    pub providers: Arc<ProviderRegistry>,
    /// Per-runtime failover manager tracking per-profile cooldowns.
    pub(crate) failover: FailoverManager,
    /// Shared per-model health table. Same `Arc` as the FailoverManager's
    /// — exposed here so tools that bypass FailoverManager (image_gen,
    /// video_gen — they POST directly to provider-specific HTTP
    /// endpoints) can still consult `is_callable` and record outcomes.
    pub(crate) model_health: rsclaw_provider::health::ProviderHealthRegistry,
    pub skills: Arc<SkillRegistry>,
    pub store: Arc<Store>,
    pub memory: Option<Arc<Mutex<MemoryStore>>>,
    pub agents: Option<Arc<AgentRegistry>>,
    /// SSE broadcast channel — None when running outside the gateway (e.g.
    /// tests).
    pub event_bus: Option<broadcast::Sender<AgentEvent>>,
    /// Shared permission store for computer_use. Same `Arc` is held by
    /// `AppState` so the WS handler can resolve pending requests minted
    /// inside an agent run. `None` outside the gateway (tests / CLI).
    pub computer_permission: Option<Arc<rsclaw_computer::permission::RedbPermissionStore>>,
    /// Broadcast channel that surfaces `PermissionRequest` to the WS
    /// gateway. The Tauri UI subscribes and shows the modal. `None`
    /// outside the gateway.
    pub computer_permission_tx:
        Option<broadcast::Sender<rsclaw_computer::permission::PermissionRequest>>,
    /// Broadcast channel that surfaces VlmDriver progress
    /// (`ComputerUseStatus::Started/Step/Finished`) to the WS gateway
    /// for the live status panel. `None` outside the gateway.
    pub computer_status_tx: Option<broadcast::Sender<rsclaw_computer::status::ComputerUseStatus>>,
    /// Shared registry of in-flight `computer_use` run abort flags.
    /// `tool_vlm_drive` inserts on driver start and removes on exit; the
    /// HTTP abort endpoint flips the bool to wake the driver loop.
    /// `None` outside the gateway.
    pub computer_runs: Option<
        Arc<
            tokio::sync::RwLock<
                std::collections::HashMap<String, Arc<std::sync::atomic::AtomicBool>>,
            >,
        >,
    >,
    /// Dynamic agent spawner — None when running outside the gateway.
    pub spawner: Option<Arc<crate::AgentSpawner>>,
    /// Plugin registry — None when running outside the gateway or with no
    /// plugins.
    pub plugins: Option<Arc<PluginRegistry>>,
    /// MCP server registry — None when no MCP servers are configured.
    pub mcp: Option<Arc<rsclaw_mcp::McpRegistry>>,
    /// WASM plugin instances for tool dispatch (shared across agents).
    pub wasm_plugins: Arc<Vec<rsclaw_plugin::WasmPlugin>>,
    /// CDP browser session -- lazy-initialized on first web_browser tool call.
    /// Stored as Option so it can be dropped (killing Chrome) when idle
    /// expires.
    pub(crate) browser: Arc<tokio::sync::Mutex<Option<rsclaw_browser::BrowserSession>>>,
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
    workspace_cache: Option<crate::workspace::WorkspaceCache>,
    /// Cached system prompt. Rebuilt on gateway restart AND whenever the
    /// workspace persona files (AGENTS.md / SOUL.md / …) or the inline
    /// `system` config change — see `cached_prompt_fingerprint`.
    pub(crate) cached_system_prompt: Option<String>,
    /// Fingerprint of the inputs `cached_system_prompt` was built from
    /// (workspace persona files + inline `system`). When it changes, the
    /// prompt is rebuilt so a freshly-created/edited SOUL.md / AGENTS.md takes
    /// effect without a gateway restart.
    cached_prompt_fingerprint: Option<u64>,
    /// Cached minimal system prompt for internal sessions
    /// (heartbeat/cron/system). Built on first internal session use.
    pub(crate) cached_minimal_prompt: Option<String>,
    /// Cached tool definitions from the last run_turn — reused by compaction
    /// to match the KV cache prefix exactly.
    pub(crate) cached_tools: Vec<rsclaw_provider::ToolDef>,
    pub(crate) notification_tx:
        Option<tokio::sync::broadcast::Sender<rsclaw_channel::OutboundMessage>>,
    /// In-memory session alias cache: alias_key → canonical session_key.
    /// Loaded from redb on first use, avoids repeated DB lookups.
    session_aliases: std::collections::HashMap<String, String>,
    /// Completed async task results: task_id → (session_key, result_json).
    /// Background task agents write here; main agent checks at turn start.
    pub(crate) pending_task_results: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    /// Sessions in voice mode: auto-TTS reply when user sent voice.
    /// Set when audio attachment detected, cleared by "/text" command.
    voice_mode_sessions: std::collections::HashSet<String>,
    /// Background exec pool — runs long commands without blocking the agent
    /// loop.
    pub(crate) exec_pool: Arc<super::exec_pool::ExecPool>,
    /// Coding-agent proxy (Claude Code, Opencode, Codex, Amp). `None` until
    /// Task 9 wires up construction; `None` outside the gateway.
    pub(crate) cap_manager: Option<std::sync::Arc<rsclaw_cap::CapAgentManager>>,
    /// Interactive multi-instance cap session manager — backs the
    /// `cap_live` / `cap_live_end` tools and the IM `/cap` direct-mode
    /// command. Shares the same driver primitives as `cap_manager` but
    /// keeps long-lived drivers keyed by session_id.
    pub(crate) cap_live_manager: Option<std::sync::Arc<rsclaw_cap::CapLiveManager>>,
    /// Server-side read cursors for `read_artifact` sequential paging.
    /// Key: `"{session_key}\u{0}{artifact_id}"` → next 1-indexed line to
    /// return. A bare `read_artifact` (no explicit mode) returns the next
    /// unread chunk and advances this cursor, so the model never has to
    /// compute `lines:A-B` ranges — calling again always makes progress and
    /// re-reading the same page is impossible. Interior-mutable because tool
    /// dispatch borrows `&self`. Entries are best-effort (lost on restart,
    /// which just resets paging to the top — harmless).
    pub(crate) artifact_cursors: std::sync::Mutex<std::collections::HashMap<String, usize>>,
}

impl AgentRuntime {
    /// Create a new agent runtime with the given configuration and
    /// dependencies.
    pub fn new(
        #[allow(clippy::too_many_arguments)] handle: Arc<AgentHandle>,
        config: Arc<RuntimeConfig>,
        live: Arc<LiveConfig>,
        providers: Arc<ProviderRegistry>,
        fallback_models: Vec<String>,
        skills: Arc<SkillRegistry>,
        store: Arc<Store>,
        memory: Option<Arc<Mutex<MemoryStore>>>,
        agents: Option<Arc<AgentRegistry>>,
        event_bus: Option<broadcast::Sender<AgentEvent>>,
        spawner: Option<Arc<crate::AgentSpawner>>,
        plugins: Option<Arc<PluginRegistry>>,
        mcp: Option<Arc<rsclaw_mcp::McpRegistry>>,
        notification_tx: Option<tokio::sync::broadcast::Sender<rsclaw_channel::OutboundMessage>>,
        model_health: rsclaw_provider::health::ProviderHealthRegistry,
        cap_manager: Option<std::sync::Arc<rsclaw_cap::CapAgentManager>>,
        cap_live_manager: Option<std::sync::Arc<rsclaw_cap::CapLiveManager>>,
    ) -> Self {
        // Populate auth.order so FailoverManager uses the configured profile
        // priority per provider (AGENTS.md §12).
        let auth_order = config
            .model
            .auth
            .as_ref()
            .and_then(|a| a.order.clone())
            .unwrap_or_default();
        // Clone the shared health registry for direct tool access. tools
        // like `image_gen` / `video_gen` build raw HTTP requests outside
        // the FailoverManager flow but still want chain-level gating —
        // they consult `self.model_health` directly. Same `Arc` so
        // FailoverManager and the tools see one source of truth.
        let model_health_for_tools = model_health.clone();
        let retry_config = config
            .model
            .retry
            .as_ref()
            .map(|r| RetryConfig {
                attempts: r.attempts.unwrap_or(3),
                min_delay_ms: r.min_delay_ms.unwrap_or(400),
                max_delay_ms: r.max_delay_ms.unwrap_or(30_000),
                jitter: r.jitter.unwrap_or(0.1),
            })
            .unwrap_or_default();
        let failover = FailoverManager::new(
            auth_order,
            std::collections::HashMap::new(),
            fallback_models,
            model_health,
            retry_config,
        );
        let session_aliases = store.db.load_all_aliases().unwrap_or_default();
        let live_status = Arc::clone(&handle.live_status);
        let max_concurrent = config.agents.defaults.max_concurrent.unwrap_or(4);
        let exec_pool = super::exec_pool::ExecPool::new(max_concurrent as usize);
        let rt = Self {
            handle,
            config,
            live,
            providers,
            failover,
            model_health: model_health_for_tools,
            skills,
            store,
            memory,
            agents,
            event_bus,
            computer_permission: None,
            computer_permission_tx: None,
            computer_status_tx: None,
            computer_runs: None,
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
            cached_prompt_fingerprint: None,
            cached_minimal_prompt: None,
            cached_tools: Vec::new(),
            pending_task_results: Arc::new(std::sync::Mutex::new(Vec::new())),
            voice_mode_sessions: std::collections::HashSet::new(),
            notification_tx,
            session_aliases,
            exec_pool,
            cap_manager,
            cap_live_manager,
            artifact_cursors: std::sync::Mutex::new(std::collections::HashMap::new()),
        };

        // Purge any internal-session history left over in redb from older
        // gateway builds (heartbeat/cron/system used to persist every
        // "HEARTBEAT_OK" reply).  These sessions are no longer persisted,
        // so drop whatever is still there.
        if let Ok(keys) = rt.store.db.list_sessions() {
            for key in keys {
                if is_internal_session(&key) {
                    if let Err(e) = rt.store.db.delete_session(&key) {
                        tracing::warn!(session = %key, error = %e, "failed to purge stale internal session");
                    }
                }
            }
        }

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

    /// Resolve the current model name from agent config with fallback.
    pub(crate) fn resolve_model_name(&self) -> String {
        self.handle
            .config
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
    }
}

// Model resolution — extracted to `model_resolution.rs`. Re-exported here so
// existing `crate::runtime::resolve_*` paths keep working.
use super::model_resolution::effective_primary_is_rsclaw;
pub use super::model_resolution::{
    VisionResolution, is_known_vision_model, model_supports_image_input, resolve_flash_model_for,
    resolve_primary_model_for, resolve_vision_model_for, vision_unavailable_message,
};

impl AgentRuntime {
    /// Estimate fixed context overhead: system prompt + tools tokens.
    /// Used for pre-flight context budget check before LLM call, and by
    /// the compaction module's estimate fallback.
    pub(crate) fn estimate_fixed_overhead(&self) -> usize {
        // Estimate system prompt from last known size (more accurate than guessing).
        let sys_tokens = self.handle.last_sys_tokens.load(Ordering::Relaxed);
        let tools_tokens = self.handle.last_tools_tokens.load(Ordering::Relaxed);
        if sys_tokens + tools_tokens > 0 {
            sys_tokens + tools_tokens
        } else {
            // Fallback: rough estimate when no LLM call has happened yet.
            // Typical system prompt ~3.5k tokens, tools ~1-2k.
            3500 + 1000
        }
    }

    /// Resolve the "flash" (cheap/fast) model used for internal sub-tasks
    /// like query planning and intent classification. Resolution order:
    ///   1. `agents.<id>.flash_model`
    ///   2. `agents.defaults.flash_model`
    ///   3. `agents.<id>.model`         (main model for this agent)
    ///   4. `agents.defaults.model`     (global default)
    /// So if no flash model is configured anywhere, we fall back to whatever
    /// the agent is already using — no regression.
    /// Resolved vision chain in preference order — analog to
    /// `resolve_vision_model_name` but returns the full ordered list of
    /// candidates instead of just the head. Lookup:
    ///   1. per-agent `model.vision` chain
    ///   2. `defaults.model.vision` chain
    /// If both are empty, falls back to primary chain (matches the
    /// legacy `FallbackToPrimary` semantics — drivers want SOMETHING
    /// vision-capable, and the agent's primary is the best guess).
    /// True when the agent's EFFECTIVE primary model is a `rsclaw/` model —
    /// the head of the per-agent primary chain when set, else the head of the
    /// defaults primary chain. Deliberately NOT "any model in either chain":
    /// a non-rsclaw per-agent primary must not inherit rsclaw just because a
    /// fallback entry or the defaults happen to be rsclaw. Drives
    /// rsclaw-protocol defaults (e.g. defaulting rerank to the fleet
    /// `rsclaw-reranker-v1`).
    pub(crate) fn primary_is_rsclaw(&self) -> bool {
        let per_agent = &self.handle.config;
        let defaults = &self.config.agents.defaults;
        per_agent
            .model
            .as_ref()
            .or(defaults.model.as_ref())
            .and_then(|m| m.primary_chain().into_iter().next())
            .map(|head| head.trim().starts_with("rsclaw/"))
            .unwrap_or(false)
    }

    pub(crate) fn resolve_vision_chain(&self) -> Vec<String> {
        let per_agent = &self.handle.config;
        let defaults = &self.config.agents.defaults;
        let mut out: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let push = |chain: Vec<&str>,
                    out: &mut Vec<String>,
                    seen: &mut std::collections::HashSet<String>| {
            for m in chain {
                let t = m.trim();
                if !t.is_empty() && seen.insert(t.to_owned()) {
                    out.push(t.to_owned());
                }
            }
        };
        if let Some(m) = per_agent.model.as_ref() {
            push(m.vision_chain(), &mut out, &mut seen);
        }
        if let Some(m) = defaults.model.as_ref() {
            push(m.vision_chain(), &mut out, &mut seen);
        }
        if out.is_empty() {
            // rsclaw-protocol default: when the primary is a `rsclaw/` model and
            // no explicit vision chain is configured, the fleet serves a
            // dedicated vision head — default to it (mirrors the flash default
            // rsclaw/rsclaw-flash-v1). Lets `vision: []` "just work" on rsclaw.
            if effective_primary_is_rsclaw(per_agent, defaults) {
                out.push(rsclaw_provider::rsclaw::RSCLAW_DEFAULT_VISION.to_owned());
            }
        }
        if out.is_empty() {
            // FallbackToPrimary: no explicit vision config, try primary
            // — but ONLY entries known to support vision. Without this
            // filter, the driver would silently route screenshots to a
            // text-only primary (e.g. deepseek/qwen-coder) and get a
            // provider error instead of the actionable
            // "configure agents.defaults.model.vision" message.
            let primary_filtered =
                |chain: Vec<&str>,
                 out: &mut Vec<String>,
                 seen: &mut std::collections::HashSet<String>| {
                    for m in chain {
                        let t = m.trim();
                        if t.is_empty() || !seen.insert(t.to_owned()) {
                            continue;
                        }
                        if is_known_vision_model(t) {
                            out.push(t.to_owned());
                        }
                    }
                };
            if let Some(m) = per_agent.model.as_ref() {
                primary_filtered(m.primary_chain(), &mut out, &mut seen);
            }
            if let Some(m) = defaults.model.as_ref() {
                primary_filtered(m.primary_chain(), &mut out, &mut seen);
            }
        }
        out
    }

    /// Split the resolved flash chain into (head, tail). Drop-in for
    /// flash LlmRequest builders: `let (model, fallback_models) =
    /// self.resolve_flash_chain_split();`. Empty head when no flash
    /// model is configured anywhere — matches `resolve_flash_model_name`
    /// returning `""` in the same case.
    pub(crate) fn resolve_flash_chain_split(&self) -> (String, Vec<String>) {
        let mut chain = self.resolve_flash_chain();
        if chain.is_empty() {
            return (String::new(), Vec::new());
        }
        let head = chain.remove(0);
        (head, chain)
    }

    /// Return the resolved flash chain — head first, fallbacks following.
    /// Callers building an `LlmRequest` can pass `chain[1..]` as
    /// `fallback_models` to enable per-call chain retry through the
    /// FailoverManager (head is still passed as `req.model`).
    pub(crate) fn resolve_flash_chain(&self) -> Vec<String> {
        let per_agent = &self.handle.config;
        let defaults = &self.config.agents.defaults;
        let mut out: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let push =
            |v: Vec<&str>, out: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
                for m in v {
                    let t = m.trim();
                    if !t.is_empty() && seen.insert(t.to_owned()) {
                        out.push(t.to_owned());
                    }
                }
            };
        if let Some(m) = per_agent.model.as_ref() {
            push(m.flash_chain(), &mut out, &mut seen);
        }
        if let Some(fm) = per_agent.flash_model.as_ref() {
            push(fm.primary_chain(), &mut out, &mut seen);
        }
        if let Some(m) = defaults.model.as_ref() {
            push(m.flash_chain(), &mut out, &mut seen);
        }
        if let Some(fm) = defaults.flash_model.as_ref() {
            push(fm.primary_chain(), &mut out, &mut seen);
        }
        if out.is_empty() {
            // RsClaw fleet inference fallback (same logic as
            // resolve_flash_model_for): if primary head is rsclaw, the
            // fleet's RSCLAW_DEFAULT_FLASH is the flash model.
            if effective_primary_is_rsclaw(per_agent, defaults) {
                out.push(rsclaw_provider::rsclaw::RSCLAW_DEFAULT_FLASH.to_owned());
            }
        }
        // Final fallback: the primary chain. Matches the legacy
        // `resolve_flash_model_name()` semantics that fell back to
        // `resolve_model_name()` (primary head) when no flash was
        // configured. Now extended to inherit the entire primary chain,
        // so flash sub-tasks gracefully share the user's failover plan.
        if out.is_empty() {
            if let Some(m) = per_agent.model.as_ref() {
                push(m.primary_chain(), &mut out, &mut seen);
            }
            if let Some(m) = defaults.model.as_ref() {
                push(m.primary_chain(), &mut out, &mut seen);
            }
        }
        out
    }

    pub(crate) fn resolve_flash_model_name(&self) -> String {
        resolve_flash_model_for(&self.handle.config, &self.config.agents.defaults)
            .unwrap_or_else(|| self.resolve_model_name())
    }

    /// Resolve the vision model for `computer_use` via the
    /// `model.vision → primary` fallback chain. Returns
    /// `Err(actionable message)` when the resolved model is known to
    /// be text-only or when nothing is configured at all — caller
    /// surfaces this directly to the user.
    ///
    /// Use this from anywhere that wants to drive a VLM-backed loop
    /// (`computer_use vlm_drive`).
    pub(crate) fn resolve_vision_model_name(&self) -> Result<String, String> {
        match resolve_vision_model_for(&self.handle.config, &self.config.agents.defaults) {
            VisionResolution::Configured(name) => Ok(name),
            VisionResolution::FallbackToPrimary(name) => {
                // 1. Honour the per-model `input` declaration in `models.providers[].models[]`
                //    first. If the user has listed `image` we trust them; if they explicitly
                //    listed only `text` we surface that as a config error.
                match model_supports_image_input(&self.config.raw, &name) {
                    Some(true) => return Ok(name),
                    Some(false) => {
                        return Err(vision_unavailable_message(&format!(
                            "model `{name}` is declared as text-only \
                             (`input: [\"text\"]`) in its provider config. \
                             Add `\"image\"` to the model's `input` array \
                             or set `agents.defaults.model.vision`."
                        )));
                    }
                    None => {} // no declaration → fall through to heuristic
                }

                // 2. No declaration: fall back to a vision-allow-list. Defaulting to text-only
                //    here is the safer choice — an unknown model name is more likely text-only
                //    than vision-capable, and a clear error pointing at the config beats a
                //    cryptic API failure later.
                if is_known_vision_model(&name) {
                    Ok(name)
                } else {
                    Err(vision_unavailable_message(&format!(
                        "primary model `{name}` is not in the built-in \
                         vision allow-list and its provider config does \
                         not declare `input: [\"image\"]`. Either set \
                         `agents.defaults.model.vision` to a vision \
                         model, or declare `input: [\"text\", \"image\"]` \
                         on the `{name}` entry under \
                         `models.providers.<provider>.models[]`."
                    )))
                }
            }
            VisionResolution::NoneConfigured => Err(vision_unavailable_message(
                "no model is configured for this agent.",
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Plugin hook dispatch (AGENTS.md §20)
    // -----------------------------------------------------------------------

    /// Fire a lifecycle hook on all plugins that subscribe to it.
    /// Errors from individual plugins are logged and swallowed — hooks must
    /// not interrupt the agent loop.
    /// Handle a 1-4 reply to the pending VIDEO menu. Options 1/3 extract
    /// the audio track and transcribe it; options 2/3 sample keyframes and
    /// run them through the vision chain. Results feed a PendingAnalysis
    /// (LLM digests transcript/description and answers in context); option
    /// 4 deletes the saved upload. ffmpeg auto-installs on first use.
    async fn handle_video_menu_choice(
        &mut self,
        choice: &str,
        files: Vec<PendingFile>,
        session_key: &str,
        channel: &str,
        peer_id: &str,
        i18n_lang: &str,
    ) -> Result<AgentReply> {
        fn direct(text: String) -> AgentReply {
            AgentReply {
                text,
                is_empty: false,
                tool_calls: None,
                images: vec![],
                files: vec![],
                pending_analysis: None,
                needs_outer_done_emit: true,
                outcome: crate::registry::ReplyOutcome::Ok,
            }
        }

        if choice == "4" {
            let names: Vec<String> = files.iter().map(|f| f.filename.clone()).collect();
            for f in &files {
                let _ = std::fs::remove_file(&f.path);
            }
            return Ok(direct(rsclaw_i18n::t_fmt(
                "video_deleted",
                i18n_lang,
                &[("names", &names.join(", "))],
            )));
        }

        let ffmpeg = match crate::platform::ensure_ffmpeg().await {
            Ok(p) => p,
            Err(e) => {
                return Ok(direct(rsclaw_i18n::t_fmt(
                    "video_no_ffmpeg",
                    i18n_lang,
                    &[("err", &format!("{e:#}"))],
                )));
            }
        };
        let want_audio = choice == "1" || choice == "3";
        let want_frames = choice == "2" || choice == "3";

        let mut sections: Vec<String> = Vec::new();
        for f in &files {
            if want_audio {
                match crate::video::extract_audio_wav(&ffmpeg, &f.path).await {
                    Ok(wav) => {
                        let client = reqwest::Client::new();
                        match rsclaw_channel::transcription::transcribe_audio(
                            &client,
                            &wav,
                            "extracted.wav",
                            "audio/wav",
                        )
                        .await
                        {
                            Ok(t) if !t.trim().is_empty() => sections.push(format!(
                                "[Video transcript: {}]\n{}",
                                f.filename,
                                t.trim()
                            )),
                            Ok(_) => sections.push(format!(
                                "[Video transcript: {} — empty/no speech]",
                                f.filename
                            )),
                            Err(e) => sections.push(format!(
                                "[Video transcript: {} — transcription failed: {e:#}]",
                                f.filename
                            )),
                        }
                    }
                    Err(e) => sections.push(format!("[Video audio: {} — {e:#}]", f.filename)),
                }
            }
            if want_frames {
                match crate::video::extract_keyframes(&ffmpeg, &f.path, 6).await {
                    Ok(frames) => {
                        let described = self.vision_describe_frames(&frames).await;
                        crate::video::cleanup_frames(&frames);
                        match described {
                            Ok(desc) => sections.push(format!(
                                "[Video frames: {} — {} keyframes]\n{}",
                                f.filename,
                                frames.len(),
                                desc
                            )),
                            Err(e) => sections.push(format!(
                                "[Video frames: {} — vision analysis failed: {e:#}]",
                                f.filename
                            )),
                        }
                    }
                    Err(e) => sections.push(format!("[Video frames: {} — {e:#}]", f.filename)),
                }
            }
        }

        // HTTP API has no async outbound sender — a PendingAnalysis result
        // would be digested and then dropped. Return the raw material
        // directly instead; chat channels keep the digest-and-push flow.
        if channel == "api" {
            return Ok(direct(sections.join("\n\n")));
        }

        let analysis = format!(
            "The user uploaded video file(s) and chose to have them analyzed. \
             Digest the material below and report to the user what the video \
             contains/says, in their language.\n\n{}",
            sections.join("\n\n")
        );
        Ok(AgentReply {
            text: rsclaw_i18n::t("analyzing", i18n_lang),
            is_empty: false,
            tool_calls: None,
            images: vec![],
            files: vec![],
            pending_analysis: Some(crate::PendingAnalysis {
                text: analysis,
                session_key: session_key.to_owned(),
                channel: channel.to_owned(),
                peer_id: peer_id.to_owned(),
            }),
            needs_outer_done_emit: true,
            outcome: crate::registry::ReplyOutcome::Ok,
        })
    }

    /// Describe a set of keyframe JPEGs with the configured vision chain.
    /// Compact one-shot — walks the chain manually (no FailoverManager,
    /// same rationale as `tool_research_analyze_charts`).
    async fn vision_describe_frames(&self, frames: &[std::path::PathBuf]) -> Result<String> {
        use base64::Engine as _;
        let chain = self.resolve_vision_chain();
        if chain.is_empty() {
            return Err(anyhow!("no vision model configured (model.vision)"));
        }
        let mut parts: Vec<ContentPart> = Vec::with_capacity(frames.len() + 1);
        parts.push(ContentPart::Text {
            text: format!(
                "These are {} keyframes sampled in order (one every ~10s) from a user-uploaded \
                 video. Describe what happens across the video: scene, people/objects, on-screen \
                 text, and how it progresses frame to frame. Be concrete.",
                frames.len()
            ),
        });
        for f in frames {
            let bytes = std::fs::read(f).map_err(|e| anyhow!("read frame {}: {e}", f.display()))?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            parts.push(ContentPart::Image {
                url: format!("data:image/jpeg;base64,{b64}"),
            });
        }
        let req = LlmRequest {
            model: chain[0].clone(),
            fallback_models: vec![],
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Parts(parts),
                rsclaw_hidden: None,
            }],
            max_tokens: Some(2048),
            temperature: Some(0.2),
            thinking_budget: Some(0),
            ..Default::default()
        };
        let providers = Arc::clone(&self.providers);
        let mut last_err = anyhow!("vision chain empty");
        for model in &chain {
            let (prov_name, model_id) = providers.resolve_model(model);
            let provider = match providers.get(prov_name) {
                Ok(p) => p,
                Err(e) => {
                    last_err = anyhow!("provider not found for {model}: {e}");
                    continue;
                }
            };
            let mut r = req.clone();
            r.model = model_id.to_owned();
            let fut = provider.stream(r);
            match tokio::time::timeout(std::time::Duration::from_secs(90), fut).await {
                Ok(Ok(mut stream)) => {
                    let mut buf = String::new();
                    while let Some(ev) = stream.next().await {
                        if let Ok(StreamEvent::TextDelta(d)) = ev {
                            buf.push_str(&d);
                        }
                    }
                    if !buf.trim().is_empty() {
                        return Ok(buf.trim().to_owned());
                    }
                    last_err = anyhow!("{model}: empty vision output");
                }
                Ok(Err(e)) => last_err = anyhow!("{model}: {e:#}"),
                Err(_) => last_err = anyhow!("{model}: timed out after 90s"),
            }
        }
        Err(last_err)
    }

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
        let btw_budget = self
            .live
            .agents
            .read()
            .await
            .defaults
            .btw_tokens
            .unwrap_or(5_000) as usize;
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
            messages.push(Message {
                role: m.role.clone(),
                content,
                rsclaw_hidden: None,
            });
            token_count += msg_tokens;
        }
        messages.reverse();
        messages.push(Message {
            role: Role::User,
            content: MessageContent::Text(question.to_owned()),
            rsclaw_hidden: None,
        });

        let model = self.resolve_model_name();
        let req = LlmRequest {
            fallback_models: Vec::new(),
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
            endpoint: AgentEndpoint::Flash,
            kv_cache_mode: 0,
            session_key: None,
            system_shared: None,
            user_system: None,
            recall: None,
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
            // /btw bypasses agent_loop — outer must emit done.
            needs_outer_done_emit: true,
            outcome: crate::registry::ReplyOutcome::Ok,
        })
    }

    /// Compress a web tool result for session storage via an ephemeral LLM
    /// call.
    ///
    /// Only the extracted answer is stored in session history — raw web content
    /// (HTML, search results, screenshots) is never concatenated into the
    /// conversation. Returns the extracted text; error means caller should fall
    /// back to plain truncation.
    /// Per-turn aggregate input guard. Caps the total ToolResult payload in
    /// `scratchpad` to `budget` tokens by trimming the largest results first.
    ///
    /// Each trimmed result keeps (or, when it has none yet, is given) a
    /// `read_artifact` handle so the model can page into the full content —
    /// this is lossless pagination, not lossy truncation. `read_artifact`'s
    /// own results are already ≤ budget (paginated in `tool_read_artifact`),
    /// so they're never the largest and pass through untouched. skill_use /
    /// other large contract docs ARE included (no exemption): a 60k-token
    /// SKILL.md sent whole would blow the kvCacheMode=2 session in one shot,
    /// so it's paged with a strong "read all pages before executing" hint.
    ///
    /// No-op when the aggregate already fits.
    async fn cap_turn_input_to_budget(
        &self,
        session_key: &str,
        scratchpad: &mut [Message],
        budget: usize,
    ) {
        use rsclaw_provider::ContentPart;

        use super::context_mgr::estimate_tokens;

        // Index every ToolResult part with its current token cost.
        let mut idx: Vec<(usize, usize, usize)> = Vec::new();
        for (mi, msg) in scratchpad.iter().enumerate() {
            if let MessageContent::Parts(parts) = &msg.content {
                for (pi, part) in parts.iter().enumerate() {
                    if let ContentPart::ToolResult { content, .. } = part {
                        idx.push((mi, pi, estimate_tokens(content)));
                    }
                }
            }
        }
        let mut total: usize = idx.iter().map(|(_, _, t)| *t).sum();
        if total <= budget {
            return;
        }

        // Floor so each trimmed result keeps a useful head + its handle.
        const MIN_RESULT_TOKENS: usize = 256;
        // Trim the biggest results first.
        idx.sort_by_key(|(_, _, t)| std::cmp::Reverse(*t));

        for (mi, pi, toks) in idx {
            if total <= budget {
                break;
            }
            let excess = total - budget;
            let target = toks.saturating_sub(excess).max(MIN_RESULT_TOKENS);
            if target >= toks {
                continue; // already at/under its share
            }
            let MessageContent::Parts(parts) = &mut scratchpad[mi].content else {
                continue;
            };
            let Some(ContentPart::ToolResult { content, .. }) = parts.get_mut(pi) else {
                continue;
            };
            let new = self
                .paginate_tool_result(session_key, content, target)
                .await;
            let new_toks = estimate_tokens(&new);
            total = total + new_toks - toks;
            *content = new;
        }
    }

    /// Trim one tool-result `content` to ~`target` tokens, preserving (or
    /// minting) a `read_artifact` handle so the full content stays
    /// recoverable. Whole-line / char-safe head via `paginate_to_budget`.
    async fn paginate_tool_result(
        &self,
        session_key: &str,
        content: &str,
        target: usize,
    ) -> String {
        use super::tools_artifact::{paginate_to_budget, split_artifact_marker};

        let (body, existing_marker) = split_artifact_marker(content);
        // Reserve ~32 tokens for the marker line so page+marker stays ≤ target.
        let page_budget = target.saturating_sub(32).max(64);
        let (page, _lines, _total) = paginate_to_budget(body, page_budget);

        if let Some(marker) = existing_marker {
            return format!("{page}{marker}");
        }
        // No handle yet — persist the full body so trimming stays lossless.
        match rsclaw_artifact::default_store().write(session_key, body) {
            Ok(id) => format!(
                "{page}\n\n[truncated to fit the per-turn input budget — full output preserved; \
                 call read_artifact(tool_result_id=\"{}\") to read the next chunk (repeat to \
                 page through it), or read_artifact(tool_result_id=\"{}\", mode=\"query:QUESTION\") \
                 to jump to the relevant part]",
                id.as_str(),
                id.as_str()
            ),
            // Artifact write failed: bounded but lossy. Mark it honestly.
            Err(e) => {
                tracing::warn!(error = %e, "per-turn guard: artifact write failed; trimming lossy");
                format!(
                    "{page}\n\n[truncated to fit the per-turn input budget; full output unavailable]"
                )
            }
        }
    }

    async fn compress_tool_result_for_session(
        &mut self,
        session_key: &str,
        tool_name: &str,
        result_text: &str,
    ) -> Result<String> {
        use super::web_parsers::html_dehydrate_to_text;

        // Step 1: extract the prose content from structured JSON results.
        // web_fetch → {url, title, text, length}; web_browser → {action, text, ...}
        let extracted = if let Ok(v) = serde_json::from_str::<serde_json::Value>(result_text) {
            v.get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.to_owned())
                .unwrap_or_else(|| result_text.to_owned())
        } else {
            result_text.to_owned()
        };

        // Step 2: strip any residual HTML.
        // web_fetch now outputs lol-html plain text, but web_browser DOM snapshots
        // can still contain HTML-like fragments.
        let clean = if extracted.contains('<') && extracted.contains('>') {
            html_dehydrate_to_text(&extracted)
        } else {
            extracted
        };

        // Step 3: cap at ~10k tokens before sending to the compression LLM.
        // 40k chars covers: ASCII-heavy (10k tokens × 4 chars) and CJK-heavy
        // (10k tokens × 1.5 chars ≈ 15k chars), with margin in between.
        const TOKEN_CAP_CHARS: usize = 40_000;
        let capped: String = clean.chars().take(TOKEN_CAP_CHARS).collect();

        // Step 4: get the user's question for context.
        let user_question: String = self
            .sessions
            .get(session_key)
            .and_then(|msgs| msgs.iter().rev().find(|m| m.role == Role::User))
            .map(|m| match &m.content {
                MessageContent::Text(t) => t.chars().take(500).collect(),
                _ => String::new(),
            })
            .unwrap_or_default();

        let prompt = if user_question.is_empty() {
            format!("Tool: {tool_name}\n\nContent:\n{capped}")
        } else {
            format!("User question: {user_question}\n\nTool ({tool_name}) returned:\n{capped}")
        };

        // Step 5: single LLM call on the flash model — raw content + the
        // user question goes in, a targeted compressed answer comes out.
        // Routes via the rsclaw fastshot endpoint
        // (`AgentEndpoint::Flash` → POST /v1/agent/fastshot) which is a
        // one-shot stateless OpenAI-compat stream — no session, no
        // kv_cache_mode, no session_key. The fastshot worker pool is
        // filtered server-side via `fastshot_enabled`, so this call
        // never competes with the primary agent's session slots.
        // Non-rsclaw providers (OpenAI, Anthropic, etc.) ignore the
        // endpoint field and just see a normal chat completion.
        // Chain-aware: when `agents.defaults.model.flash` is a multi-entry
        // chain, the tail rides as `fallback_models` so the same per-model
        // health gating kicks in for fastshot calls.
        let (model, flash_fallbacks) = self.resolve_flash_chain_split();
        let req = LlmRequest {
            fallback_models: flash_fallbacks,
            model,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(prompt),
                rsclaw_hidden: None,
            }],
            tools: vec![],
            system: Some(
                "You are an information extractor. Given tool output and a user question, \
                 extract the facts that directly answer the question. \
                 Output structured plain text: a direct answer paragraph, then bullet points \
                 for key facts. No HTML, no JSON, no code blocks. \
                 If the content does not answer the question, summarize what was found in \
                 1-2 sentences. Reply in the same language as the user's question."
                    .to_owned(),
            ),
            max_tokens: Some(1000),
            temperature: None,
            frequency_penalty: None,
            thinking_budget: None,
            endpoint: AgentEndpoint::Flash,
            kv_cache_mode: 0,
            session_key: None,
            system_shared: None,
            user_system: None,
            recall: None,
        };
        // session_key keeps it lints-quiet now that fastshot doesn't
        // route through a stateful session — callers still pass one
        // and we may revive use for telemetry / cache-tag later.
        let _ = session_key;

        let providers = Arc::clone(&self.providers);
        let mut stream = self.failover.call(req, &providers).await?;
        let mut buf = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(d)) => buf.push_str(&d),
                Ok(StreamEvent::Done { .. }) | Ok(StreamEvent::Error(_)) => break,
                Ok(_) => {}
                Err(e) => return Err(anyhow!("compress stream error: {e}")),
            }
        }

        if buf.is_empty() {
            return Err(anyhow!("empty response from compression LLM"));
        }
        Ok(buf)
    }

    /// Caption incoming user-attached images via the agent's `vision` slot.
    ///
    /// Used when the agent's *primary* model is text-only (e.g.
    /// `rsclaw-agent-v1` configured as primary while doubao-seed-2.0-lite
    /// sits in the `vision` slot). Without this hop, base64 image data
    /// would be shipped to a model that ignores it (silent hallucination)
    /// or rejects it (provider 400). Instead, fan out to the vision slot,
    /// get a text description, and let the primary continue against the
    /// description.
    ///
    /// Returns the caption text on success. On any failure (no vision
    /// chain configured, provider error, empty output, timeout) returns
    /// `Err` and the caller is expected to fall back to pass-through
    /// (preserves the previous behaviour for fully-vision primaries that
    /// could read the image directly anyway).
    async fn caption_images_for_text_only_primary(
        &mut self,
        user_text: &str,
        images: &[super::registry::ImageAttachment],
    ) -> Result<String> {
        let vision_chain = self.resolve_vision_chain();
        let vision_model = vision_chain
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("no vision model in chain"))?;

        // Build the multimodal prompt. The vision model's job is NOT to
        // answer the user's question — just describe the image precisely
        // so the primary can answer based on the description.
        let prompt = format!(
            "Describe the attached image(s) in detail. Be precise about \
             visible text (quote verbatim), UI elements, layout, colors, \
             positions, and any error messages or structural cues. Multiple \
             images: describe each separately with a 'Image N:' heading.\n\n\
             Do NOT analyze, interpret, or answer any question — just \
             describe what is literally visible. Another AI will use your \
             description to answer the user.\n\n\
             For context, the user's request was: {}",
            user_text
        );

        let mut parts = vec![ContentPart::Text { text: prompt }];
        for img in images {
            parts.push(ContentPart::Image {
                url: img.data.clone(),
            });
        }

        let vision_model_name = vision_model.clone();
        let req = LlmRequest {
            model: vision_model,
            fallback_models: vision_chain.into_iter().skip(1).collect(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Parts(parts),
                rsclaw_hidden: None,
            }],
            max_tokens: Some(2048),
            temperature: Some(0.3),
            thinking_budget: Some(0),
            ..Default::default()
        };

        let providers = Arc::clone(&self.providers);
        let stream_fut = self.failover.call(req, &providers);
        let mut stream = tokio::time::timeout(Duration::from_secs(45), stream_fut)
            .await
            .map_err(|_| anyhow!("vision caption timed out (45s)"))??;

        // Collect text AND reasoning separately. Some vision workers stream the
        // description as `thinking` frames (→ ReasoningDelta) rather than text
        // deltas even with thinking_budget=0; in that case the description is
        // still the answer we want, so fall back to it when no text arrived.
        // Without this fallback the caption silently came back empty and the
        // whole image turn degraded to "vision recognition failed".
        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut frame_count: usize = 0;
        while let Some(event) = stream.next().await {
            frame_count += 1;
            match event {
                Ok(StreamEvent::TextDelta(d)) => text_buf.push_str(&d),
                Ok(StreamEvent::ReasoningDelta(d)) => reasoning_buf.push_str(&d),
                Ok(StreamEvent::Done { .. }) => break,
                Ok(StreamEvent::Error(msg)) => bail!("vision caption: {msg}"),
                Ok(_) => {}
                Err(e) => return Err(anyhow!("vision caption stream error: {e}")),
            }
        }

        let out = if !text_buf.trim().is_empty() {
            text_buf
        } else if !reasoning_buf.trim().is_empty() {
            debug!(model = %vision_model_name, "vision caption: using reasoning frames (no text deltas)");
            reasoning_buf
        } else {
            bail!("vision caption: empty output (model={vision_model_name}, frames={frame_count})");
        };
        Ok(out)
    }

    /// Drive a single conversation turn.
    ///
    /// Takes individual fields (not the full `AgentMessage`) so callers can
    /// extract `reply_tx` separately before dispatching.
    /// True if `agent_id` runs as a long-lived daemon (forever monitor loop).
    /// The gateway uses this to give daemon turns a far longer stuck-turn
    /// watchdog limit — they're *meant* to run indefinitely, so the normal
    /// 20-min cap would needlessly cancel them and dark the queue.
    pub fn is_daemon_agent(&self, agent_id: &str) -> bool {
        self.config.agents.is_daemon_agent(agent_id)
    }

    /// Resolve a session key through the alias table.
    /// Reload installed skills from disk and invalidate the cached system
    /// prompt so newly-installed (or removed) skills appear in the per-session
    /// "## Installed Skills" list. Cache-safe: skills live in user_system, not
    /// the base-layer hash, so this only re-prefills that per-session layer —
    /// and only when the byte-stable prompt actually changed. Called at the
    /// natural refresh points (compact / clear / new).
    pub(crate) fn reload_skills(&mut self) {
        let dir = rsclaw_config::loader::base_dir().join("skills");
        match rsclaw_skill::load_skills(&dir, None, self.config.ext.skills.as_ref()) {
            Ok(reg) => {
                let before = self.skills.all().count();
                self.skills = Arc::new(reg);
                let after = self.skills.all().count();
                if before != after {
                    // Skill set changed → drop the cached prompt so the next
                    // turn rebuilds user_system with the new skill list.
                    self.cached_system_prompt = None;
                    tracing::info!(before, after, "reloaded skills (set changed)");
                }
            }
            Err(e) => tracing::warn!("reload_skills failed: {e:#}"),
        }
    }

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
    /// Called before `/new` — since no summary is injected into the new
    /// session, memory is the only way the LLM can find prior context.
    /// Uses KV cache mode when available (session is still in memory).
    async fn save_session_summaries_to_memory(&mut self, model: &str) {
        if self.memory.is_none() {
            return;
        }

        let kv_cache_mode = self
            .live
            .agents
            .read()
            .await
            .defaults
            .kv_cache_mode
            .unwrap_or(1);

        // Collect session data upfront to avoid borrow conflicts.
        let session_data: Vec<(String, String)> = self
            .sessions
            .iter()
            .filter(|(_, msgs)| msgs.len() > 2)
            .map(|(key, msgs)| {
                let transcript = Self::msgs_to_text_static(msgs, 16_000);
                (key.clone(), transcript)
            })
            .collect();

        for (session_key, transcript) in &session_data {
            // Generate summary — try KV cache mode first.
            let summary = if kv_cache_mode >= 1 {
                let result = self
                    .compact_with_kv_cache(session_key, model, transcript, None)
                    .await;
                if result.is_some() {
                    result
                } else {
                    self.compact_single(model, transcript, None).await
                }
            } else {
                self.compact_single(model, transcript, None).await
            };

            let Some(summary) = summary else { continue };

            // Store as a session_summary memory doc.
            let scope = format!("agent:{}", self.handle.id);
            let doc = crate::memory::MemoryDoc {
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

    /// Load session history from in-memory cache, falling back to redb.
    /// Session key should already be resolved through `resolve_session_key`
    /// (done in `run_turn`) so aliases are transparent.
    ///
    /// Internal sessions (heartbeat/cron/system) are never persisted to redb
    /// (see `is_internal_session`) — always start with an empty history so
    /// stale entries from a previous version don't leak in.
    fn load_session(&mut self, session_key: &str) -> &mut Vec<Message> {
        if !self.sessions.contains_key(session_key) {
            let history = if is_internal_session(session_key) {
                Vec::new()
            } else {
                self.store
                    .db
                    .load_messages(session_key)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|v| serde_json::from_value::<Message>(v).ok())
                    .collect::<Vec<_>>()
            };
            self.sessions.insert(session_key.to_owned(), history);
        }
        self.sessions.get_mut(session_key).expect("just inserted")
    }

    // -----------------------------------------------------------------------
    // Workflow crystallization trigger
    // -----------------------------------------------------------------------

    /// Inspect this turn's metrics and, if the difficulty score crosses
    /// the configured threshold, spawn a background workflow distillation.
    /// Skipped silently when:
    ///   - `[ext.evolution.workflow]` is disabled
    ///   - tool_calls below `min_tool_calls`
    ///   - tool_errors below `min_errors`
    ///   - difficulty score below threshold
    ///   - rate limit exceeded OR signature already crystallized this run
    fn maybe_crystallize_workflow(&self, ctx: &RunContext, final_text: &str) {
        let evo = crate::evolution::evolution_config();
        if !evo.enabled || !evo.workflow.enabled {
            return;
        }
        let m = &ctx.turn_metrics;
        if m.tool_calls < evo.workflow.min_tool_calls {
            return;
        }
        if m.tool_errors < evo.workflow.min_errors {
            return;
        }
        let score = m.difficulty_score();
        if score < evo.workflow.score_threshold {
            return;
        }
        let signature = m.signature();
        if !crate::turn_metrics::try_admit_workflow(signature, evo.workflow.max_per_hour) {
            tracing::debug!(
                signature,
                score,
                "workflow crystallization: dedup or rate-limit, skipping"
            );
            return;
        }

        // Snapshot the data the background task needs — agent_loop's stack
        // frame goes away as soon as we return.
        let providers = Arc::clone(&self.providers);
        let model = self.resolve_flash_model_name();
        let skills_dir = rsclaw_skill::default_global_skills_dir()
            .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("skills"));
        let user_text = ctx.user_text.clone();
        let reply_text = final_text.to_owned();
        let metrics = ctx.turn_metrics.clone();

        tracing::info!(
            score,
            tool_calls = m.tool_calls,
            tool_errors = m.tool_errors,
            distinct_tools = m.distinct_tools.len(),
            "spawning workflow crystallization"
        );
        tokio::spawn(async move {
            match rsclaw_skill::workflow_distill::crystallize_workflow(
                &user_text,
                &reply_text,
                &metrics,
                signature,
                &providers,
                &model,
                &skills_dir,
            )
            .await
            {
                Ok(Some(_path)) => { /* logged inside crystallize_workflow */ }
                Ok(None) => {
                    // Distillation skipped (kill-switch / model issue / LLM
                    // error / validation failure). Roll the signature back
                    // so a future retry isn't blocked by the dedup set.
                    crate::turn_metrics::release_signature(signature);
                }
                Err(e) => {
                    tracing::warn!("workflow crystallization hard failure: {e:#}");
                    crate::turn_metrics::release_signature(signature);
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // Organic evolution helpers
    // -----------------------------------------------------------------------

    /// Infer an outcome signal from the completed turn.
    ///
    /// Returns a value in \[-0.3, 0.3\]: positive = helpful, negative =
    /// unhelpful. Used by Loop A to adjust importance of recalled memories.
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

    /// A2A v1.0 INPUT_REQUIRED / AUTH_REQUIRED bridge tool.
    ///
    /// The agent calls this when it needs the client to supply more text
    /// (e.g. a credential, a confirmation, a missing parameter) before it
    /// can finish the turn. The runtime publishes a TASK_STATE_INPUT_REQUIRED
    /// (or AUTH_REQUIRED if `auth=true`) status event with `prompt` as the
    /// agent-role message, registers a one-shot resume handle on
    /// `state.suspended_tasks`, and awaits the client's reply.
    ///
    /// Resume protocol: the client sends a fresh SendMessage /
    /// SendStreamingMessage with the **same taskId** and the new text. The
    /// dispatcher detects the existing `SuspendedTask` entry, pops it, and
    /// pushes the text into the `resume_tx`. This tool then returns the
    /// text as its result and the agent loop continues.
    ///
    /// Non-A2A turns (TurnContext default) have no `input_request_tx`, so
    /// this returns an error — the LLM can recover with a plain reply.
    pub(crate) async fn tool_wait_input(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("Please provide additional input to continue.");
        let auth = args.get("auth").and_then(|v| v.as_bool()).unwrap_or(false);
        if ctx.turn_ctx.input_request_tx.is_none() {
            return Ok(json!({
                "error": "wait_input is only supported on A2A turns",
            }));
        }
        match ctx.turn_ctx.request_input(prompt, auth).await {
            Some(text) => Ok(json!({ "input": text })),
            None => Err(anyhow!("resume channel dropped while awaiting input")),
        }
    }

    /// Escalate the user's current request into a multi-turn background
    /// task. The LLM is the one judging when this is warranted (see the
    /// `task` ToolDef description). The original `looks_like_task`
    /// keyword heuristic regularly mis-classified short Chinese
    /// questions like "你可以帮我做啥？", so the decision moved here.
    pub(crate) async fn tool_task(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        use rsclaw_types::{
            Priority, QueuedMessage, TASK_DEFAULT_MAX_TURNS, TASK_DEFAULT_TTL_SECS,
        };
        let task_text = args
            .get("task_text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if task_text.is_empty() {
            return Ok(json!({
                "error": "task_text is required and must be non-empty"
            }));
        }
        let max_turns = args
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(TASK_DEFAULT_MAX_TURNS);
        let ttl_secs = args
            .get("ttl_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(TASK_DEFAULT_TTL_SECS);

        let Some(host) = rsclaw_types::task_queue_host() else {
            return Ok(json!({
                "error": "task queue not available (gateway not fully started?)"
            }));
        };

        let message = QueuedMessage {
            text: task_text.clone(),
            sender: ctx.peer_id.clone(),
            channel: ctx.channel.clone(),
            chat_id: ctx.chat_id.clone(),
            is_group: false,
            reply_to: None,
            timestamp: chrono::Utc::now().timestamp(),
            images: Vec::new(),
            files: Vec::new(),
            account: None,
        };

        let (task_id, merged) = host
            .submit_task(
                &ctx.session_key,
                message,
                Priority::User,
                max_turns,
                ttl_secs,
            )
            .map_err(|e| anyhow!("failed to submit task: {e:#}"))?;

        Ok(json!({
            "task_id": task_id,
            "merged": merged,
            "max_turns": max_turns,
            "ttl_secs": ttl_secs,
            "next_step": "Reply to the user with a brief acknowledgement only \
                          (e.g. 'Started, will report progress'). The actual \
                          work runs in the background and posts updates as \
                          turns complete."
        }))
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

// expand_tilde + canonicalize_external_path lifted to rsclaw-util
// (crate-split); re-exported.
pub(crate) use rsclaw_util::{canonicalize_external_path, expand_tilde};

/// Single source of truth for the per-agent default workspace path used by
/// file tools (`list_dir`, `search_file`, `search_content`, `read_file`,
/// `write_file`, `edit_file`, `shell`) when the caller didn't pass an
/// explicit `path`. Resolution order:
///
///   1. The agent's own `config.workspace` override (per-agent dir)
///   2. The global `agents.defaults.workspace` (gateway-wide default)
///   3. `<base_dir>/workspace` (built-in fallback — the `main` agent uses this)
///
/// Always returns an absolute `PathBuf` (tilde-expanded). Critically NEVER
/// returns `.` or the gateway's CWD — a regression there would make every
/// agent's file ops escape its workspace.
pub(crate) fn resolve_default_workspace(
    agent_workspace: Option<&str>,
    defaults_workspace: Option<&str>,
    base_dir: &std::path::Path,
) -> std::path::PathBuf {
    agent_workspace
        .or(defaults_workspace)
        .map(expand_tilde)
        .unwrap_or_else(|| base_dir.join("workspace"))
}

fn resolve_request_max_tokens(
    from_agent: Option<u32>,
    from_defaults: Option<u32>,
    from_provider: Option<u32>,
    provider_name: &str,
    model_id: &str,
) -> Option<u32> {
    for configured in [from_agent, from_defaults, from_provider] {
        if let Some(value) = configured {
            return (value > 0).then_some(value);
        }
    }
    let resolved =
        rsclaw_provider::model_defaults::resolve_max_tokens(provider_name, model_id, None);
    (resolved > 0).then_some(resolved)
}

// ---------------------------------------------------------------------------
// File extraction helpers (FileAttachment gate)
// ---------------------------------------------------------------------------

/// Format a tool call result as human-readable markdown.
/// Walk a `Value` and replace any string longer than `max_chars` with a
/// `[N chars]` placeholder. Used by Phase 2b's streaming preview so a
/// tool that returns a giant base64 image / full-page HTML doesn't dump
/// the raw payload into the UI bus marker. Non-mutating: returns a
/// fresh `Value`.
fn squash_large_strings(val: &serde_json::Value, max_chars: usize) -> serde_json::Value {
    use serde_json::Value;
    match val {
        Value::String(s) => {
            let chars = s.chars().count();
            if chars > max_chars {
                Value::String(format!("[{chars} chars elided]"))
            } else {
                Value::String(s.clone())
            }
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| squash_large_strings(v, max_chars))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), squash_large_strings(v, max_chars));
            }
            Value::Object(out)
        }
        _ => val.clone(),
    }
}

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
            // Title fallback chain widened: skill_search returns
            // {slug, description, installs, registry} (no `title`), so
            // every result was rendering as "(no title)". Try the common
            // identifier fields any result-shaped payload exposes.
            let title = r
                .get("title")
                .or_else(|| r.get("source_title"))
                .or_else(|| r.get("doc_title"))
                .or_else(|| r.get("summary"))
                .or_else(|| r.get("content"))
                .or_else(|| r.get("slug"))
                .or_else(|| r.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("(no title)");
            let url = r
                .get("url")
                .or_else(|| r.get("doc_id"))
                .or_else(|| r.get("chunk_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let snippet = r
                .get("snippet")
                .or_else(|| r.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
    use rsclaw_config::config_json::{load_config_json, set_nested_value};

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
            if rsclaw_util::get_nested_value(&val, &prefix).is_none() {
                set_nested_value(&mut val, &prefix, serde_json::json!({}))?;
            }
        }
    }

    set_nested_value(&mut val, dot_path, value)?;
    std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool dispatch helpers — inject fields for backward-compat routing
// ---------------------------------------------------------------------------

/// Build the "## rsclaw helpers" cheatsheet appended to the cap subagent's
/// first-turn prompt. Lists the cross-process CLI commands the subagent
/// can call via bash to reach rsclaw's memory store, knowledge base,
/// installed plugins, and (lightweight, name-only) skill library.
///
/// The cheatsheet intentionally lists CLI commands rather than raw curl
/// templates: cap subagents drive bash natively, and the `rsclaw` binary
/// wraps auth tokens + JSON shell-escaping uniformly across macOS / Linux
/// / Windows (Windows' curl/PowerShell alias quirks would otherwise leak
/// into every example). Empty string when there are no skills AND we
/// can't introduce the CLI surface meaningfully — though in practice the
/// CLI surface is always present, so this only returns "" if the agent
/// has skills disabled by config.
fn build_cap_helper_cheatsheet(skills: &Arc<rsclaw_skill::SkillRegistry>) -> String {
    let mut sections: Vec<String> = Vec::new();

    // Always include the rsclaw CLI helpers — they're the most useful
    // tools a coding subagent has into the rest of the user's stack.
    sections.push(
        "## rsclaw helpers (run via bash; auth/URL handled internally)\n\n\
         ```\n\
         # Memory — persistent facts/preferences the main agent has learned.\n\
         rsclaw memory search \"<query>\" [--max-results N] [--json]\n\
         rsclaw memory save \"<fact>\" [--scope SCOPE] [--kind fact|note] [--pinned] [--json]\n\n\
         # Knowledge base — ingested docs/URLs (semantic + BM25 hybrid).\n\
         rsclaw kb search \"<query>\" [-k N] [--json]\n\
         rsclaw kb add <path-or-url> [--tag T ...] [--recursive] [--ext glob]\n\n\
         # Plugins — list/describe/invoke installed plugin tools.\n\
         rsclaw plugins list\n\
         rsclaw plugins describe <plugin>\n\
         rsclaw plugins call <plugin>.<tool> --args '{\"k\":\"v\"}'\n\n\
         # Messaging — send/read/broadcast through the IM channels rsclaw is wired to.\n\
         rsclaw message send --channel <wechat|feishu|telegram|...> --target <id> -m \"...\"\n\
         rsclaw message read --channel <ch> --target <id> [--limit N] [--json]\n\
         rsclaw message broadcast --channel <ch> --targets <id1> --targets <id2> -m \"...\"\n\
         ```\n\n\
         All commands print JSON with --json. Run any with --help for full flags."
            .to_owned(),
    );

    // Skill library — name + one-line description so the subagent can
    // decide if any recipe matches, then Read the SKILL.md file from
    // disk on its own. Keeping the cheatsheet at name-level (not
    // pasting SKILL.md content) keeps token cost bounded.
    let skill_root = rsclaw_skill::default_global_skills_dir();
    let mut entries: Vec<(String, String)> = skills
        .all()
        .filter_map(|m| {
            let desc = m
                .description
                .as_deref()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_owned();
            // Skip skills without a name (shouldn't happen but be safe).
            if m.name.is_empty() {
                None
            } else {
                Some((m.name.clone(), desc))
            }
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if !entries.is_empty() {
        let mut lines = String::new();
        lines.push_str("## Skill library (read SKILL.md for usage; not auto-invoked)\n\n");
        if let Some(root) = skill_root.as_ref() {
            lines.push_str(&format!(
                "Skill files live under `{}`. Read the SKILL.md when a name looks relevant.\n\n",
                root.display()
            ));
        }
        for (name, desc) in entries.iter().take(50) {
            if desc.is_empty() {
                lines.push_str(&format!("- **{name}**\n"));
            } else {
                lines.push_str(&format!("- **{name}** — {desc}\n"));
            }
        }
        sections.push(lines);
    }

    sections.join("\n")
}

/// Maximum characters to send from file content to LLM.
#[allow(dead_code)]
const MAX_FILE_CONTENT_CHARS: usize = 20_000;

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Persist dynamic agent to config file
// ---------------------------------------------------------------------------

/// Patch fields of an existing `AgentEntry` in `agents.list` in the config
/// file.
///
/// - `model`: `Some("")` or `Some("default")` removes the field (agent falls
///   back to defaults). `Some("provider/model")` sets `model.primary`. `None`
///   leaves it untouched.
/// - `name`:  `Some("")` removes the field. `Some(x)` sets it. `None` leaves
///   it.
///
/// The config hot-reload watcher picks up the change automatically — no restart
/// needed.
pub(crate) async fn update_agent_in_config(
    id: &str,
    model: Option<&str>,
    name: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    use serde_json::json;

    let config_path = rsclaw_config::loader::detect_config_path()
        .ok_or_else(|| anyhow!("no config file found"))?;
    let raw = tokio::fs::read_to_string(&config_path).await?;
    let mut doc: serde_json::Value =
        json5::from_str(&raw).map_err(|e| anyhow!("parse config: {e}"))?;

    let list = doc
        .pointer_mut("/agents/list")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("agents.list not found in config"))?;

    let entry = list
        .iter_mut()
        .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(id))
        .ok_or_else(|| anyhow!("agent not found: {id}"))?;

    let mut changes: Vec<String> = vec![];

    if let Some(m) = model {
        if m.is_empty() || m == "default" {
            if entry
                .as_object_mut()
                .and_then(|o| o.remove("model"))
                .is_some()
            {
                changes.push("model removed (falls back to defaults)".to_owned());
            }
        } else {
            entry["model"] = json!({"primary": m});
            changes.push(format!("model → {m}"));
        }
    }

    if let Some(n) = name {
        if n.is_empty() {
            entry.as_object_mut().map(|o| o.remove("name"));
        } else {
            entry["name"] = json!(n);
        }
        changes.push("name updated".to_owned());
    }

    if changes.is_empty() {
        return Ok(json!({"warning": "nothing to update — provide model and/or name"}));
    }

    let output = serde_json::to_string_pretty(&doc)?;
    tokio::fs::write(&config_path, output).await?;
    tracing::info!(agent_id = %id, ?changes, "agent config updated");

    Ok(json!({
        "updated": id,
        "changes": changes,
        "note": "saved — hot-reload applies within seconds"
    }))
}

/// Append an AgentEntry to the `agents.list` array in the config file.
/// The hot-reload watcher will pick up the change automatically.
pub(crate) async fn persist_agent_to_config(
    entry: &rsclaw_config::schema::AgentEntry,
) -> anyhow::Result<()> {
    let config_path = rsclaw_config::loader::detect_config_path()
        .ok_or_else(|| anyhow!("no config file found"))?;
    let raw = tokio::fs::read_to_string(&config_path).await?;
    let mut doc: serde_json::Value =
        json5::from_str(&raw).map_err(|e| anyhow!("parse config: {e}"))?;

    // Don't duplicate if agent already exists.
    let id = entry.id.as_str();
    let already_exists = doc
        .pointer("/agents/list")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .any(|e| e.get("id").and_then(|v| v.as_str()) == Some(id))
        })
        .unwrap_or(false);
    if already_exists {
        return Ok(());
    }

    let mut entry_val = serde_json::to_value(entry)?;

    // Strip model field if it matches agents.defaults.model.primary
    // (no need to persist what the defaults already provide).
    let defaults_primary = doc
        .pointer("/agents/defaults/model/primary")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let entry_primary = entry_val
        .pointer("/model/primary")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
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

fn intermediate_notification_text(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
