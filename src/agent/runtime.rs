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

use crate::skill::SkillManifest;

fn paginate_skill_list<'a, I>(skills: I, args: &Value) -> Value
where
    I: IntoIterator<Item = &'a SkillManifest>,
{
    let query = args["query"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let query_lower = query.map(|q| q.to_lowercase());
    let limit = args["limit"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(50)
        .clamp(1, 100);
    let offset = args["offset"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);

    let mut all = skills.into_iter().collect::<Vec<_>>();
    let total = all.len();
    all.retain(|s| {
        let Some(query) = query_lower.as_deref() else {
            return true;
        };
        s.name.to_lowercase().contains(query)
            || s.description
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(query)
    });
    all.sort_by(|a, b| a.name.cmp(&b.name));

    let matched = all.len();
    let skills = all
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description.clone().unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    let next_offset = offset + skills.len();
    let has_more = next_offset < matched;

    json!({
        "count": total,
        "matched": matched,
        "offset": offset,
        "limit": limit,
        "has_more": has_more,
        "next_offset": has_more.then_some(next_offset),
        "skills": skills,
    })
}

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

pub use super::context_mgr::estimate_tokens;
use super::{
    context_mgr::{
        apply_context_budget_trim, apply_context_pruning, build_clear_summary, msg_tokens,
    },
    loop_detection::LoopDetector,
    memory::{MemDocTier, MemoryDoc, MemoryStore},
    prompt_builder::{
        READONLY_COMMANDS, build_help_text_filtered, build_minimal_system_prompt,
        build_system_prompt, format_duration, memory_age_label,
    },
    registry::{AgentHandle, AgentMessage, AgentRegistry, AgentReply},
    security::check_read_safety,
    tool_call_repair::repair_tool_result_pairing,
    tools_builder::{build_tool_list, toolset_allowed_names},
    workspace::{DEFAULT_MAX_CHARS_PER_FILE, DEFAULT_TOTAL_MAX_CHARS, SessionType},
};
use crate::{
    config::runtime::RuntimeConfig,
    events::AgentEvent,
    gateway::live_config::LiveConfig,
    plugin::PluginRegistry,
    provider::{
        AgentEndpoint, ContentPart, LlmRequest, Message, MessageContent, RecallBundle,
        RecallMetadata, Role, StreamEvent, ToolDef, failover::FailoverManager,
        registry::ProviderRegistry,
    },
    skill::{RunOptions, SkillRegistry, run_tool},
    store::Store,
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
pub(crate) fn parse_qualified_tool(entry: &str) -> Option<(String, String)> {
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

/// Format one active-plugin-tool line for the "## Active Plugin Tools"
/// block. Schema is serialized compactly via `serde_json` so a 20-tool
/// block stays under ~3-4 KB. The schema JSON object's keys are emitted
/// in `serde_json` declaration order (the crate is compiled with
/// `preserve_order`), so byte-stability for a given input is up to the
/// upstream tool registry — not normalized here because this block lives
/// in `user_system` (NOT hashed), so cross-session byte-divergence has
/// no KV-cache impact.
#[allow(dead_code)]
fn format_active_plugin_tool_line(name: &str, description: &str, schema: &Value) -> String {
    let one_line_desc = description.trim().replace('\n', " ");
    let schema_compact = serde_json::to_string(schema)
        .unwrap_or_else(|_| r#"{"type":"object","properties":{}}"#.to_owned());
    format!("- **{name}** — {one_line_desc}\n  input_schema: {schema_compact}")
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
    pub(crate) model_health: crate::provider::health::ProviderHealthRegistry,
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
    pub computer_permission: Option<Arc<crate::computer::permission::RedbPermissionStore>>,
    /// Broadcast channel that surfaces `PermissionRequest` to the WS
    /// gateway. The Tauri UI subscribes and shows the modal. `None`
    /// outside the gateway.
    pub computer_permission_tx:
        Option<broadcast::Sender<crate::computer::permission::PermissionRequest>>,
    /// Broadcast channel that surfaces VlmDriver progress
    /// (`ComputerUseStatus::Started/Step/Finished`) to the WS gateway
    /// for the live status panel. `None` outside the gateway.
    pub computer_status_tx: Option<broadcast::Sender<crate::computer::status::ComputerUseStatus>>,
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
    pub spawner: Option<Arc<crate::agent::AgentSpawner>>,
    /// Plugin registry — None when running outside the gateway or with no
    /// plugins.
    pub plugins: Option<Arc<PluginRegistry>>,
    /// MCP server registry — None when no MCP servers are configured.
    pub mcp: Option<Arc<crate::mcp::McpRegistry>>,
    /// WASM plugin instances for tool dispatch (shared across agents).
    pub wasm_plugins: Arc<Vec<crate::plugin::WasmPlugin>>,
    /// CDP browser session -- lazy-initialized on first web_browser tool call.
    /// Stored as Option so it can be dropped (killing Chrome) when idle
    /// expires.
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
    /// Cached minimal system prompt for internal sessions
    /// (heartbeat/cron/system). Built on first internal session use.
    pub(crate) cached_minimal_prompt: Option<String>,
    /// Cached tool definitions from the last run_turn — reused by compaction
    /// to match the KV cache prefix exactly.
    pub(crate) cached_tools: Vec<crate::provider::ToolDef>,
    pub(crate) notification_tx:
        Option<tokio::sync::broadcast::Sender<crate::channel::OutboundMessage>>,
    pub(crate) opencode_client: Arc<tokio::sync::OnceCell<crate::acp::client::AcpClient>>,
    pub(crate) claudecode_client: Arc<tokio::sync::OnceCell<crate::acp::client::AcpClient>>,
    pub(crate) codex_client: Arc<tokio::sync::OnceCell<crate::acp::CodexClient>>,
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
}

impl AgentRuntime {
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
        spawner: Option<Arc<crate::agent::AgentSpawner>>,
        plugins: Option<Arc<PluginRegistry>>,
        mcp: Option<Arc<crate::mcp::McpRegistry>>,
        notification_tx: Option<tokio::sync::broadcast::Sender<crate::channel::OutboundMessage>>,
        model_health: crate::provider::health::ProviderHealthRegistry,
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
        let failover = FailoverManager::new(
            auth_order,
            std::collections::HashMap::new(),
            fallback_models,
            model_health,
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
            cached_minimal_prompt: None,
            cached_tools: Vec::new(),
            pending_task_results: Arc::new(std::sync::Mutex::new(Vec::new())),
            voice_mode_sessions: std::collections::HashSet::new(),
            notification_tx,
            opencode_client: Arc::new(tokio::sync::OnceCell::new()),
            claudecode_client: Arc::new(tokio::sync::OnceCell::new()),
            codex_client: Arc::new(tokio::sync::OnceCell::new()),
            session_aliases,
            exec_pool,
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

    // OpenCode / Claude Code ACP integration -> moved to tools_acp.rs

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

/// Resolve the primary model name from per-agent + defaults config,
/// without needing an `AgentRuntime` instance. Returns `None` if nothing
/// is configured — caller decides on a fallback.
///
/// Lookup chain mirrors [`AgentRuntime::resolve_model_name`]:
///   1. per-agent `model.primary`
///   2. `defaults.model.primary`
pub fn resolve_primary_model_for(
    per_agent: &crate::config::schema::AgentEntry,
    defaults: &crate::config::schema::AgentDefaults,
) -> Option<String> {
    per_agent
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .or_else(|| defaults.model.as_ref().and_then(|m| m.primary_head()))
        .map(str::to_owned)
}

/// Resolve the flash (cheap/fast) model name from per-agent + defaults config,
/// without needing an `AgentRuntime` instance. Returns `None` if no flash or
/// primary model is configured anywhere — the caller decides on a fallback.
///
/// Same lookup chain as [`AgentRuntime::resolve_flash_model_name`]:
///   1. per-agent `model.flash`
///   2. per-agent `flash_model.primary` (legacy)
///   3. `defaults.model.flash`
///   4. `defaults.flash_model.primary` (legacy)
///   5. **RsClaw provider inference**: if the effective primary lives under the
///      `rsclaw/` namespace (managed fleet), auto-pick
///      [`RSCLAW_DEFAULT_FLASH`]. Saves users from having to repeat the rsclaw
///      flash slot when their primary already names a rsclaw model — the same
///      "convention over configuration" treatment the provider gets for
///      api/baseUrl/prefix_id.
pub fn resolve_flash_model_for(
    per_agent: &crate::config::schema::AgentEntry,
    defaults: &crate::config::schema::AgentDefaults,
) -> Option<String> {
    let explicit = per_agent
        .model
        .as_ref()
        .and_then(|m| m.flash_head())
        .or_else(|| {
            per_agent
                .flash_model
                .as_ref()
                .and_then(|m| m.primary_head())
        })
        .or_else(|| defaults.model.as_ref().and_then(|m| m.flash_head()))
        .or_else(|| {
            defaults
                .flash_model
                .as_ref()
                .and_then(|m| m.primary_head())
        })
        .map(str::to_owned);
    if explicit.is_some() {
        return explicit;
    }

    // RsClaw fleet inference (see RSCLAW_DEFAULT_FLASH).
    let primary = per_agent
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .or_else(|| defaults.model.as_ref().and_then(|m| m.primary_head()));
    if let Some(p) = primary {
        if p.starts_with("rsclaw/") {
            return Some(crate::provider::rsclaw::RSCLAW_DEFAULT_FLASH.to_owned());
        }
    }
    None
}

/// Outcome of vision-model resolution. The four cases are distinguished
/// so the caller can format actionable error messages — "you have no
/// vision model AND no primary, configure one", vs "your primary
/// {name} is text-only, set agents.defaults.model.vision".
#[derive(Debug, Clone)]
pub enum VisionResolution {
    /// An explicit `model.vision` was configured (per-agent or in
    /// defaults). The string is the model identifier.
    Configured(String),
    /// No `vision` set; falling back to the agent's primary model.
    /// Caller may want to verify the primary actually supports images
    /// before proceeding.
    FallbackToPrimary(String),
    /// Neither `vision` nor `primary` is configured anywhere — the
    /// runtime can't proceed. Caller surfaces a "configure
    /// agents.defaults.model.{vision,primary}" message.
    NoneConfigured,
}

/// Resolve the vision model for `computer_use` (and any other VLM-backed
/// path). Lookup chain:
///
///   1. per-agent `model.vision`
///   2. `defaults.model.vision`
///   3. per-agent `model.primary`
///   4. `defaults.model.primary`
///
/// (1) and (2) return `Configured`. (3) and (4) return
/// `FallbackToPrimary`. Nothing → `NoneConfigured`.
pub fn resolve_vision_model_for(
    per_agent: &crate::config::schema::AgentEntry,
    defaults: &crate::config::schema::AgentDefaults,
) -> VisionResolution {
    if let Some(name) = per_agent
        .model
        .as_ref()
        .and_then(|m| m.vision_head())
        .map(str::to_owned)
    {
        return VisionResolution::Configured(name);
    }
    if let Some(name) = defaults
        .model
        .as_ref()
        .and_then(|m| m.vision_head())
        .map(str::to_owned)
    {
        return VisionResolution::Configured(name);
    }

    // No explicit vision configured. Before declaring a primary fallback,
    // check if the primary lives under the `rsclaw/` namespace — in that
    // case the fleet exposes a dedicated vision slot
    // ([`RSCLAW_DEFAULT_VISION`]) which is what the user almost
    // certainly wants. The default `rsclaw/rsclaw-agent-v1` primary is
    // text-only, so falling back to it would surface a "primary is
    // text-only" error to the user even though a perfectly good vision
    // slot is sitting one HTTP hop away. Treat the inferred rsclaw
    // vision model as `Configured` (not `FallbackToPrimary`) so callers
    // skip the text-only check below — the rsclaw fleet vouches for it.
    let primary = per_agent
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .or_else(|| defaults.model.as_ref().and_then(|m| m.primary_head()));
    if let Some(p) = primary {
        if p.starts_with("rsclaw/") {
            return VisionResolution::Configured(
                crate::provider::rsclaw::RSCLAW_DEFAULT_VISION.to_owned(),
            );
        }
    }

    if let Some(name) = per_agent
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .map(str::to_owned)
    {
        return VisionResolution::FallbackToPrimary(name);
    }
    if let Some(name) = defaults
        .model
        .as_ref()
        .and_then(|m| m.primary_head())
        .map(str::to_owned)
    {
        return VisionResolution::FallbackToPrimary(name);
    }
    VisionResolution::NoneConfigured
}

/// Look up `model_name` (e.g. `"kimi/kimi-for-coding"` or just
/// `"kimi-for-coding"`) in the provider config and return whether its
/// `input` array contains `image`. Returns:
///   - `Some(true)` — explicitly declared as image-capable.
///   - `Some(false)` — explicitly declared as text-only (no `image` in the
///     array).
///   - `None` — no `models[].input` entry found; caller should fall back to the
///     blocklist heuristic.
///
/// The lookup is fuzzy: it tries `provider/model_id` first (when the
/// name contains `/`), then falls back to scanning every provider for a
/// matching `model.id`. This way users who write `"kimi-for-coding"`
/// (without provider prefix) still get the declaration honoured.
pub fn model_supports_image_input(
    config: &crate::config::schema::Config,
    model_name: &str,
) -> Option<bool> {
    use crate::config::schema::InputType;

    let models_cfg = config.models.as_ref()?;
    let (prov_name, model_id) = match model_name.split_once('/') {
        Some((p, m)) => (Some(p), m),
        None => (None, model_name),
    };

    // Closure: probe one provider's models[] for a matching id.
    let probe = |entries: &Option<Vec<crate::config::schema::ModelDef>>| {
        entries.as_ref().and_then(|defs| {
            defs.iter()
                .find(|d| d.id == model_id)
                .and_then(|d| d.input.as_ref())
                .map(|inputs| inputs.contains(&InputType::Image))
        })
    };

    // Targeted lookup first.
    if let Some(prov) = prov_name {
        if let Some(pc) = models_cfg.providers.get(prov) {
            if let Some(verdict) = probe(&pc.models) {
                return Some(verdict);
            }
        }
    }

    // Otherwise scan every provider — first hit wins.
    for pc in models_cfg.providers.values() {
        if let Some(verdict) = probe(&pc.models) {
            return Some(verdict);
        }
    }
    None
}

/// Heuristic substring list of model names known to be **vision-capable**
/// (accept image input). When the schema-driven check
/// (`models.providers[].models[].input` array) is missing, the resolver
/// falls back to this allow-list. Models NOT in this list are treated as
/// text-only by default — safer than the inverse (an unknown new model
/// is more likely text-only than vision-capable, and forcing the user
/// to opt in by either listing it here or declaring `input: ["image"]`
/// in their config produces a clear error message instead of a cryptic
/// API failure later).
///
/// Match is `model.to_lowercase().contains(s)`. Add a substring when
/// you've confirmed a model family ships with image input.
pub fn is_known_vision_model(model: &str) -> bool {
    let m = model.to_lowercase();
    [
        // -------- universal suffixes (covers most "-vision" / "-vl"
        // -------- variants across vendors without per-model entries)
        "-vision",
        "-vl-",
        "-vl/",
        "-vl:",
        "-omni",
        // -------- OpenAI
        "gpt-4o",
        "gpt-4-vision",
        "gpt-4-turbo",
        "gpt-4.1",
        "gpt-5",
        "chatgpt-4o",
        "o1-",
        "o3-",
        "o4-",
        // (bare "gpt-4" intentionally NOT included — original GPT-4 base is text-only)

        // -------- Anthropic Claude 3+
        "claude-3",
        "claude-sonnet-4",
        "claude-opus-4",
        "claude-haiku-4",
        "claude-4",
        "claude-5",
        // (claude-instant / claude-2 are text-only)

        // -------- Google Gemini + Gemma 3+
        "gemini-1.5",
        "gemini-2",
        "gemini-3",
        "gemini-pro-vision",
        "gemma-3",
        "gemma-4",
        "paligemma",
        // (gemma-1/-2 text-only)

        // -------- Meta Llama (3.2 vision + Llama 4 multimodal)
        "llama-3.2-11b-vision",
        "llama-3.2-90b-vision",
        "llama-3.2-vision",
        "llama-4",
        // (llama-3 / llama-3.1 / llama-3.3 / llama-3.2-1b / llama-3.2-3b are text-only)

        // -------- Mistral
        "pixtral",
        "mistral-small-3.1",
        "mistral-small-3.2",
        "mistral-small-4",
        "mistral-medium-3",
        // -------- Cohere
        "aya-vision",
        "command-a-vision",
        // -------- xAI Grok (3+ natively multimodal; older variants need -vision)
        "grok-2-vision",
        "grok-1.5-vision",
        "grok-3",
        "grok-4",
        "grok-5",
        // -------- ByteDance Doubao
        // Seed 1.x: required `-vision` suffix to be multimodal.
        "doubao-seed-1.5-vision",
        "doubao-1.5-vision",
        "doubao-1-5-vision",
        "doubao-seed-1.6-vision",
        // Seed 2+ family: entire subtree is multimodal-by-default
        // (pro / lite / code / flash / vision all accept image input).
        // List 2..=9 explicitly so future generations (3.x, 4.x, ...)
        // are auto-recognised without a code change.
        "doubao-seed-2",
        "doubao-seed-3",
        "doubao-seed-4",
        "doubao-seed-5",
        "doubao-seed-6",
        "doubao-seed-7",
        "doubao-seed-8",
        "doubao-seed-9",
        // Other vision lines.
        "doubao-pro-vision",
        "doubao-vision",
        "seedream",
        "seedance",
        // -------- Alibaba Qwen
        "qwen-vl",
        "qwen2-vl",
        "qwen2.5-vl",
        "qwen3-vl",
        "qwen-max-vision",
        // Qwen 3.5+ base series multimodal; both spellings.
        "qwen3.5",
        "qwen-3.5",
        "qwen3.6",
        "qwen-3.6",
        "qwen3.7",
        "qwen-3.7",
        "qwen3.8",
        "qwen-3.8",
        "qwen3.9",
        "qwen-3.9",
        "qwen4",
        "qwen-4",
        "qvq", // Qwen visual-question
        // -------- Moonshot Kimi
        "kimi-for-coding",
        "kimi-k2.5",
        "kimi-k2.6",
        "kimi-k2.7",
        "kimi-k2.8",
        "kimi-k2.9",
        "kimi-vl",
        "moonshot-v1-vision",
        // -------- Zhipu GLM (look for "vN" suffix — glm-4v, glm-4.5v, ...)
        "glm-4v",
        "glm-4.1v",
        "glm-4.5v",
        "glm-4.6v",
        "glm-5v",
        "cogvlm",
        "cogagent",
        // -------- Baidu ERNIE
        "ernie-vl",
        "ernie-4.5-vl",
        "ernie-5",
        "ernie-vision",
        // -------- SenseTime SenseChat
        "sensechat-vision",
        "sensechat-v",
        "sensenova-v6",
        // -------- 01.AI Yi
        "yi-vl",
        "yi-vision",
        // -------- Baichuan
        "baichuan-omni",
        "baichuan-vl",
        "baichuan2-vl",
        // -------- DeepSeek
        "deepseek-vl",
        "deepseek-vl2",
        "janus",
        // -------- Tencent Hunyuan
        "hunyuan-vision",
        "hunyuan-vl",
        "hunyuanocr",
        // -------- MiniMax
        // NOTE: M2 / M2.5 / M2.7 base models are TEXT-ONLY despite
        // marketing claims of "native multimodality" — confirmed by
        // Artificial Analysis (artificialanalysis.ai) and the official
        // model card on build.nvidia.com (text input only). Only the
        // explicitly vision-tagged variants accept images.
        "minimax-vl",
        "abab-vision",
        "abab6.5-vision",
        // -------- StepFun
        "step-1v",
        "step-1o",
        "step-2-vision",
        "step-3",
        "step-3.5",
        // -------- Open-source major VLMs
        "llava",
        "internvl",
        "mini-internvl",
        "xcomposer",
        "minicpm-v",
        "minicpm-o",
        "minicpm-llama3-v",
        "phi-3-vision",
        "phi-3.5-vision",
        "phi-4-multimodal",
        "idefics",
        "blip",
        "instructblip",
        "xgen-mm",
        "fuyu",
        "kosmos",
        "ferret",
        "openelm-vision",
        "mm1",
        "florence-2",
        "florence-vl",
        "smolvlm",
        "vila",
        "nvila",
        "eagle2",
        "nvlm",
        "nemotron-vl",
        "pali-3",
        // -------- GUI-agent / screen-understanding VLMs (RsClaw's core
        //          user community — keep this list eager)
        "ui-tars",
        "showui",
        "os-atlas",
        "seeclick",
        "screenagent",
        "aria-ui",
        "omniparser",
        "mobileagent",
        "appagent",
        "autoui",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// User-facing error message emitted when vision-model resolution lands
/// on a configuration that can't drive `computer_use`. Localised — the
/// gateway language is read from `crate::i18n::default_lang()` so the
/// message reaches the user in the channel they configured (Feishu /
/// WeChat / Telegram / etc.). Falls back to English when the language
/// is unset.
pub fn vision_unavailable_message(reason: &str) -> String {
    let lang = crate::i18n::default_lang();
    crate::i18n::t_fmt("vision_unavailable", lang, &[("reason", reason)])
}

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
            // FallbackToPrimary: no explicit vision config, try primary
            // — but ONLY entries known to support vision. Without this
            // filter, the driver would silently route screenshots to a
            // text-only primary (e.g. deepseek/qwen-coder) and get a
            // provider error instead of the actionable
            // "configure agents.defaults.model.vision" message.
            let primary_filtered = |chain: Vec<&str>,
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
        let push = |v: Vec<&str>, out: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
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
            let primary_head = per_agent
                .model
                .as_ref()
                .and_then(|m| m.primary_head())
                .or_else(|| defaults.model.as_ref().and_then(|m| m.primary_head()));
            if let Some(p) = primary_head {
                if p.starts_with("rsclaw/") {
                    out.push(crate::provider::rsclaw::RSCLAW_DEFAULT_FLASH.to_owned());
                }
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
            outcome: crate::agent::registry::ReplyOutcome::Ok,
        })
    }

    /// Compress a web tool result for session storage via an ephemeral LLM
    /// call.
    ///
    /// Only the extracted answer is stored in session history — raw web content
    /// (HTML, search results, screenshots) is never concatenated into the
    /// conversation. Returns the extracted text; error means caller should fall
    /// back to plain truncation.
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
            bail!(
                "vision caption: empty output (model={vision_model_name}, frames={frame_count})"
            );
        };
        Ok(out)
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
        chat_id: &str,
        extra_tools: Vec<ToolDef>,
        images: Vec<super::registry::ImageAttachment>,
        files: Vec<super::registry::FileAttachment>,
        turn_ctx: super::registry::TurnContext,
    ) -> Result<AgentReply> {
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
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));
        let resolved = crate::channel::resolve_file_refs(text, &workspace);
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
                let (final_bytes, final_mime) = crate::util::downscale_image_for_vision(
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
                images.push(super::registry::ImageAttachment {
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
                .or(self.live.agents.read().await.defaults.workspace.as_deref())
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
                            let subdir = crate::channel::upload_subdir(&pf.mime_type, &pf.filename);
                            binary_kept.push((pf.filename.clone(), subdir.to_string()));
                        }
                        let _ = std::fs::remove_file(&pf.path);
                    }
                    // Binary-only: direct reply, no LLM.
                    if analysis_text.is_empty() {
                        let msg = binary_kept
                            .iter()
                            .map(|(name, subdir)| {
                                let suffix = crate::i18n::t_fmt(
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
                            outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                        // pending_analysis short-circuit bypasses agent_loop.
                        needs_outer_done_emit: true,
                        outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                            // File-handling short-circuit bypasses agent_loop.
                            needs_outer_done_emit: true,
                            outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                        // pending_analysis short-circuit bypasses agent_loop.
                        needs_outer_done_emit: true,
                        outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                        // File-handling short-circuit bypasses agent_loop.
                        needs_outer_done_emit: true,
                        outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                        let lang = self
                            .config
                            .raw
                            .gateway
                            .as_ref()
                            .and_then(|g| g.language.as_deref())
                            .map(crate::i18n::resolve_lang)
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
                            crate::provider::registry::ProviderRegistry::parse_model(&model);
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
                                // Single read guard for both defaults — the
                                // prior code held two consecutive
                                // `self.live.agents.read().await` calls a few
                                // tokens apart. Compaction-on-/clear runs on
                                // the user-input path so the contention is
                                // observable, not theoretical.
                                let (context_tokens, cfg) = {
                                    let agents = self.live.agents.read().await;
                                    (
                                        agents.defaults.context_tokens.unwrap_or(64_000) as usize,
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
                                role: crate::provider::Role::User,
                                content: crate::provider::MessageContent::Text(format!(
                                    "[Session summary before /clear]\n{summary}"
                                )),
                                rsclaw_hidden: None,
                            };
                            self.sessions.insert(session_key.to_owned(), vec![msg]);
                        }
                        crate::i18n::t("session_cleared", crate::i18n::default_lang()).to_owned()
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
                                        Ok(_) => info!(
                                            "compact: summary saved to memory ({} chars)",
                                            mem_text.len()
                                        ),
                                        Err(e) => warn!("compact: failed to save to memory: {e}"),
                                    }
                                }
                                crate::i18n::t("compact_done", crate::i18n::default_lang())
                                    .to_owned()
                            } else {
                                crate::i18n::t(
                                    "compact_done_no_summary",
                                    crate::i18n::default_lang(),
                                )
                                .to_owned()
                            }
                        } else {
                            crate::i18n::t("compact_nothing", crate::i18n::default_lang())
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
                        let zh = crate::i18n::default_lang() == "zh";
                        if zh {
                            "已切换到文字回复模式。".to_owned()
                        } else {
                            "Switched to text reply mode.".to_owned()
                        }
                    }
                    "__VOICE_MODE__" => {
                        self.voice_mode_sessions.insert(session_key.to_owned());
                        let zh = crate::i18n::default_lang() == "zh";
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
                        let cron_path = crate::config::loader::base_dir().join("cron.json5");
                        let jobs = crate::agent::tools_cron::read_cron_jobs(&cron_path).await;
                        crate::agent::tools_cron::format_cron_jobs(&jobs)
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
                            outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                        outcome: crate::agent::registry::ReplyOutcome::Ok,
                    });
                }
                // Fall through to LLM for unhandled directives
            }
            crate::agent::preparse::PreParseResult::ToolCall { tool, args }
                if cmd_permitted(text) =>
            {
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
                        outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                            auto_recall: None,
                            loop_warning_triggered: false,
                            loop_failure: None,
                            turn_metrics: super::turn_metrics::TurnMetrics::new(),
                            user_text: String::new(),
                            full_trace: None,
                            turn_ctx: super::registry::TurnContext::default(),
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
                            outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                            outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                        needs_outer_done_emit: true,
                        outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                        needs_outer_done_emit: true,
                        outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                    needs_outer_done_emit: true,
                    outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                outcome: crate::agent::registry::ReplyOutcome::Ok,
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
            crate::channel::is_audio_attachment(&f.mime_type, &f.filename)
                && !crate::channel::is_video_attachment(&f.mime_type, &f.filename)
        });
        let mut files = regular_files;

        // Convert NEW images to FileAttachments so they go through the
        // unified pending-file flow (save → menu → user choice).
        // Skip this for:
        //   - @-referenced images (already on disk, going to vision)
        //   - Inline image+text turns (desktop/ws/a2a/api callers that pack
        //     a question alongside the image; the user clearly wants this
        //     image analysed THIS turn — the save-and-ask menu is friction
        //     left over from old single-payload channels like feishu/wechat
        //     where image-message and text-message arrive separately)
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
                    files.push(super::registry::FileAttachment {
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
                crate::channel::is_audio_attachment(&f.mime_type, &f.filename)
                    && !crate::channel::is_video_attachment(&f.mime_type, &f.filename)
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
                    chat_id,
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
        if !files.is_empty() {
            let ws = agent_cfg
                .workspace
                .as_deref()
                .or(self.live.agents.read().await.defaults.workspace.as_deref())
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
                    // File-size-exceeded short-circuit bypasses agent_loop.
                    needs_outer_done_emit: true,
                    outcome: crate::agent::registry::ReplyOutcome::Ok,
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
                    // Disk-low short-circuit bypasses agent_loop.
                    needs_outer_done_emit: true,
                    outcome: crate::agent::registry::ReplyOutcome::Ok,
                });
            }

            let mut file_info = Vec::new();
            for file in files {
                // Route to type-specific subdirectory with standardized filename.
                let subdir = crate::channel::upload_subdir(&file.mime_type, &file.filename);
                let std_name = crate::channel::upload_filename(&file.mime_type, &file.filename);
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
                } else if crate::channel::is_video_attachment(&file.mime_type, &file.filename)
                    || crate::channel::is_audio_attachment(&file.mime_type, &file.filename)
                {
                    None
                } else {
                    extract_file_text(&file.filename, &file.data).await
                };
                let has_text = extracted.is_some();
                let est_tokens = extracted.as_ref().map(|t| estimate_tokens(t)).unwrap_or(0);

                file_info.push((std_name.clone(), size, has_text, est_tokens));

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

            let saved_msg = if i18n_lang == "zh" {
                format!("已保存 {} 个文件:", file_info.len())
            } else {
                format!("{} file(s) saved:", file_info.len())
            };
            let any_analyzable = file_info.iter().any(|(_, _, has_text, _)| *has_text);
            let menu_msg = if any_analyzable {
                crate::i18n::t("file_menu", i18n_lang)
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
                outcome: crate::agent::registry::ReplyOutcome::Ok,
            });
        }

        // (Old two-layer image/text gate removed -- files handled above)

        // Workspace path — expand leading `~/` so dynamically spawned agents work.
        let workspace = agent_cfg
            .workspace
            .as_deref()
            .or(self.live.agents.read().await.defaults.workspace.as_deref())
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
                let prompt = build_system_prompt(
                    &ws_ctx,
                    &self.skills,
                    &self.wasm_plugins,
                    self.plugins.as_deref(),
                    &self.config.raw,
                );
                // DEBUG: dump full system prompt to file for inspection
                if std::env::var("RSCLAW_DUMP_PROMPT").is_ok() {
                    let dump_path =
                        crate::config::loader::base_dir().join("debug_system_prompt.txt");
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
            chain
                .into_iter()
                .skip(1)
                .map(String::from)
                .collect()
        };
        let (model_provider, _) = self.providers.resolve_model(&model);

        // Loop A (organic evolution): collect recalled memory IDs for feedback.
        // Auto-recall is turn-local committed recall. It is enabled only for
        // native rsclaw sessions; external providers do not understand hidden
        // replay state and must not persist rsclaw_hidden they never consumed.
        let auto_recall_bundle = if model_provider == "rsclaw" {
            self.build_auto_recall_bundle(&self.handle.id, channel, text)
                .await
        } else {
            None
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
            // user_tools_cap default — 30 fits ~10 headlines × 3 plugins
            // without overflowing a 64k-context small model.
            let cap = model_cfg
                .and_then(|m| m.user_tools_cap)
                .unwrap_or(DEFAULT_USER_TOOLS_CAP);
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
            let selections = Self::select_user_tools_pure(
                &self.wasm_plugins,
                self.plugins.as_deref(),
                &session_overrides,
                config_pin,
                config_unpin,
                cap,
            );
            for sel in selections {
                let wire_name = sel.wire_name();
                all.push(crate::provider::ToolDef {
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
            let dump_path = crate::config::loader::base_dir()
                .join(format!("debug_prompt_spec.{safe_key}.json"));

            let tool_json = |t: &crate::provider::ToolDef| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            };
            let mut builtin_tools = Vec::new();
            let mut user_tools = Vec::new();
            for t in &tools {
                if crate::agent::prompt_builder::BUILTIN_TOOL_NAMES.contains(&t.name.as_str()) {
                    builtin_tools.push(tool_json(t));
                } else {
                    user_tools.push(tool_json(t));
                }
            }

            let shared_prefix = crate::agent::prompt_builder::build_shared_system_prefix();
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
        //   1. `extract_file_refs` parses `[file:/abs/path]` markers
        //      (desktop drag-drop / paste path-references) and stores
        //      base64-encoded bytes in `IncomingMsg.images` server-side.
        //   2. `resolve_file_refs` parses `@<src>_<kind>_<id>.<ext>`
        //      markers and produces `resolved.image_paths` (legacy uploads).
        // For the current turn we decide where the bytes go:
        //   - If the primary model is vision-capable: pass-through into
        //     `vision_images_for_current_turn` so it sees the image
        //     directly. Most accurate; fast.
        //   - If the primary model is text-only: fan out to the agent's
        //     `vision` slot for a caption, then inject the caption as a
        //     text media-description. The primary then runs against text
        //     only — saves base64 on the wire, keeps KV cache hot, and
        //     stops the silent-hallucination failure mode where the
        //     primary model receives image_url content it can't read and
        //     either ignores it (faking a description) or keyword-routes
        //     to a tool whose name happens to match the filename (e.g.
        //     `computer_use action=screenshot` for `screenshot.png`).
        //   - On caption failure (no vision chain configured, timeout,
        //     provider error) fall back to pass-through. Worse than a
        //     successful caption, but better than dropping the image.
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
                                format!("(路径: {})", paths.join(", "))
                            }
                        };
                        media_descriptions.push(format!(
                            "用户上传了 {n} 张图片{path_hint},但 vision 模型识别失败 ({e})。\
                             请告诉用户图片无法解析,建议稍后重试,或换工具重新引用同一路径。",
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
            rsclaw_hidden: auto_recall_bundle
                .as_ref()
                .and_then(RecallBundle::to_rsclaw_hidden),
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

        // Timeout wrapper.
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
                outcome: crate::agent::registry::ReplyOutcome::Ok,
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
            turn_metrics: super::turn_metrics::TurnMetrics::new(),
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

        let reply = time::timeout(
            Duration::from_secs(timeout_secs),
            self.agent_loop(
                &mut ctx,
                &model,
                primary_chain_tail.clone(),
                &system_prompt,
                tools,
                extra_tools,
                abort_flag.clone(),
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
                let providers = Arc::clone(&self.providers);
                let model = self.resolve_flash_model_name();
                // Crystallized skills go to the global skill directory so the
                // existing load_skills() call sites pick them up on next reload.
                let skills_dir = crate::skill::default_global_skills_dir()
                    .unwrap_or_else(|| crate::config::loader::base_dir().join("skills"));
                let scope = format!("agent:{}", self.handle.id);
                tokio::spawn(async move {
                    for doc_id in candidates {
                        if let Err(e) = crate::skill::crystallizer::crystallize_one(
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
            let user_entities = crate::agent::context_mgr::extract_key_entities(text);
            if !user_entities.is_empty() {
                let docs = crate::agent::context_mgr::write_entity_memories(
                    mem,
                    &doc_scope,
                    user_entities,
                )
                .await;
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
            if crate::agent::memory_extractor::salience_gate(text) {
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
                    crate::agent::memory_extractor::extract_l1(
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
            if crate::agent::memory_extractor::correction_gate(text) {
                let mem_clone = Arc::clone(mem);
                let providers = Arc::clone(&self.providers);
                let model = self.resolve_flash_model_name();
                let scope = doc_scope.clone();
                let user_text = text.to_owned();
                tokio::spawn(async move {
                    crate::agent::memory_extractor::extract_lesson(
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
                    crate::agent::memory_extractor::extract_failure_lesson(
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
            let sherpa_tts_bin = crate::config::loader::base_dir()
                .join("tools")
                .join("sherpa-onnx")
                .join("bin")
                .join(if cfg!(target_os = "windows") {
                    "sherpa-onnx-offline-tts.exe"
                } else {
                    "sherpa-onnx-offline-tts"
                });
            let has_vits_dir = std::fs::read_dir(crate::config::loader::base_dir().join("models"))
                .map(|entries| {
                    entries.flatten().any(|e| {
                        e.path().is_dir() && e.file_name().to_string_lossy().starts_with("vits-")
                    })
                })
                .unwrap_or(false);
            let sherpa_tts_ready = sherpa_tts_bin.exists() && has_vits_dir;
            if !sherpa_tts_ready && super::install_hints::claim_first_hint("tts-sherpa") {
                let lang = crate::i18n::default_lang();
                reply
                    .text
                    .push_str(&crate::i18n::t("install_hint_tts_sherpa", lang));
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

    /// Resolve a session key through the alias table.
    /// Reload installed skills from disk and invalidate the cached system
    /// prompt so newly-installed (or removed) skills appear in the per-session
    /// "## Installed Skills" list. Cache-safe: skills live in user_system, not
    /// the base-layer hash, so this only re-prefills that per-session layer —
    /// and only when the byte-stable prompt actually changed. Called at the
    /// natural refresh points (compact / clear / new).
    pub(crate) fn reload_skills(&mut self) {
        let dir = crate::config::loader::base_dir().join("skills");
        match crate::skill::load_skills(&dir, None, None) {
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
        let evo = crate::agent::evolution::evolution_config();
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
        if !crate::agent::turn_metrics::try_admit_workflow(signature, evo.workflow.max_per_hour) {
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
        let skills_dir = crate::skill::default_global_skills_dir()
            .unwrap_or_else(|| crate::config::loader::base_dir().join("skills"));
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
            match crate::skill::workflow_distill::crystallize_workflow(
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
                    crate::agent::turn_metrics::release_signature(signature);
                }
                Err(e) => {
                    tracing::warn!("workflow crystallization hard failure: {e:#}");
                    crate::agent::turn_metrics::release_signature(signature);
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // Core agent loop
    // -----------------------------------------------------------------------

    /// `primary_chain_tail` is the rest of the primary chain after `model`
    /// (the head). Empty for single-model configs — preserves legacy
    /// single-model + global-fallback behaviour. The FailoverManager
    /// reads `LlmRequest.fallback_models` populated from this list.
    async fn agent_loop(
        &mut self,
        ctx: &mut RunContext,
        model: &str,
        primary_chain_tail: Vec<String>,
        system_prompt: &str,
        tools: Vec<ToolDef>,
        extra_tools: Vec<ToolDef>,
        abort_flag: Arc<AtomicBool>,
    ) -> Result<AgentReply> {
        // Pull both defaults the agent loop's prelude needs under a
        // single read guard. Previously these were two adjacent
        // `self.live.agents.read().await` calls — once for
        // `context_pruning`, once for `context_tokens` — paying the
        // RwLock acquisition cost twice on every agent_loop entry.
        let (pruning_cfg, defaults_context_tokens) = {
            let agents = self.live.agents.read().await;
            (
                agents.defaults.context_pruning.clone(),
                agents.defaults.context_tokens,
            )
        };

        // Resolve context budget (tokens) for history trimming.
        // Priority: agent model config > defaults.contextTokens >
        // defaults.model.contextTokens > 128000
        let context_tokens = self
            .handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.context_tokens)
            .or(defaults_context_tokens)
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
        let mut tool_log: Vec<(String, String, String)> = Vec::new();

        // Scratch-paper buffer for this turn's tool-call/tool-result messages.
        //
        // Tool calls and their results are "working notes" — they are needed by
        // the LLM during the current turn but should NOT pollute the persistent
        // session history.  Only the final assistant text reply is stored in
        // self.sessions / redb; everything else lives here and is discarded when
        // the turn ends.
        let mut turn_scratchpad: Vec<Message> = Vec::new();

        // Inject completed async task results into the session.
        {
            let mut pending = self
                .pending_task_results
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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
                            rsclaw_hidden: None,
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
        let pending_results = self
            .exec_pool
            .collect_pending_for_session(&ctx.session_key)
            .await;
        if !pending_results.is_empty() {
            info!(session = %ctx.session_key, count = pending_results.len(), "exec_pool: collected pending results");
            if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                // Collect existing ToolUse IDs in session
                let session_tool_ids: std::collections::HashSet<String> = sess
                    .iter()
                    .filter_map(|m| {
                        if m.role == Role::Assistant {
                            if let MessageContent::Parts(parts) = &m.content {
                                Some(
                                    parts
                                        .iter()
                                        .filter_map(|p| {
                                            if let ContentPart::ToolUse { id, .. } = p {
                                                Some(id.clone())
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<Vec<_>>(),
                                )
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .flatten()
                    .collect();

                // Find running ToolResults to replace
                let running_ids: std::collections::HashSet<String> = sess
                    .iter()
                    .filter_map(|m| {
                        if m.role == Role::Tool {
                            if let MessageContent::Parts(parts) = &m.content {
                                for p in parts {
                                    if let ContentPart::ToolResult {
                                        tool_use_id,
                                        content,
                                        ..
                                    } = p
                                    {
                                        if content.contains("\"status\": \"running\"") {
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
                let ids_to_replace: std::collections::HashSet<String> = pending_results
                    .iter()
                    .map(|r| r.tool_call_id.clone())
                    .filter(|id| running_ids.contains(id))
                    .collect();
                if !ids_to_replace.is_empty() {
                    sess.retain(|m| {
                        if m.role == Role::Tool {
                            if let MessageContent::Parts(parts) = &m.content {
                                for p in parts {
                                    if let ContentPart::ToolResult {
                                        tool_use_id,
                                        content,
                                        ..
                                    } = p
                                    {
                                        if ids_to_replace.contains(tool_use_id)
                                            && content.contains("\"status\": \"running\"")
                                        {
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
                            rsclaw_hidden: None,
                        });
                    }
                    let is_error = result.exit_code.map(|c| c != 0).unwrap_or(true);
                    let content = serde_json::json!({
                        "exit_code": result.exit_code,
                        "stdout": result.stdout,
                        "stderr": result.stderr,
                    })
                    .to_string();
                    sess.push(Message {
                        role: Role::Tool,
                        content: MessageContent::Parts(vec![ContentPart::ToolResult {
                            tool_use_id: tool_call_id,
                            content,
                            is_error: Some(is_error),
                        }]),
                        rsclaw_hidden: None,
                    });
                }
            }
        }

        // Dynamic iteration limit based on task complexity.
        // Default: 20 iterations. Complex tools (browser/opencode/exec): up to
        // configured max.
        const BASE_ITERATIONS: usize = 20;
        let configured_complex: usize = self
            .live
            .agents
            .read()
            .await
            .defaults
            .max_iterations
            .map(|v| v as usize)
            .unwrap_or(30);
        // Track consecutive identical tool calls (same name + same args).
        let mut last_tool_key = String::new();
        let mut same_call_streak: usize = 0;
        const MAX_SAME_CALL_STREAK: usize = 5;
        // Track consecutive tool errors — stop early when tools keep failing.
        let mut error_streak: usize = 0;
        const MAX_ERROR_STREAK: usize = 5;
        // Store last error info so we can surface it when the loop breaks.
        let mut last_error_info: Option<String> = None;
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
                let terminal_text = "[session cleared]".to_string();
                if let Some(ref bus) = self.event_bus {
                    let _ = bus.send(AgentEvent {
                        session_id: ctx.session_key.clone(),
                        agent_id: ctx.agent_id.clone(),
                        delta: terminal_text.clone(),
                        done: true,
                        files: vec![],
                        images: vec![],
                        tool_log: vec![],
                        question: None,
                    });
                }
                return Ok(AgentReply {
                    text: terminal_text,
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    needs_outer_done_emit: false,
                    outcome: crate::agent::registry::ReplyOutcome::Ok,
                });
            }
            // Check A2A cancel_token at start of each iteration. Same intent
            // as the user-side /abort path below, just from a different source
            // (the AppState.task_cancels entry that handle_cancel_task fires).
            // Returns Err so the gateway worker reports `canceled by A2A
            // CancelTask` and the dispatcher publishes TaskState::Canceled.
            if ctx.turn_ctx.is_cancelled() {
                info!(session = %ctx.session_key, iteration, "agent_loop: canceled by A2A");
                return Err(anyhow!("canceled by A2A CancelTask"));
            }
            // Check abort flag at start of each iteration (allows /abort to
            // interrupt even when tool dispatch is blocking between LLM calls).
            if abort_flag.load(Ordering::SeqCst) {
                abort_flag.store(false, Ordering::SeqCst);
                info!(session = %ctx.session_key, iteration, "agent_loop: aborted by user");
                let terminal_text = "[aborted]".to_string();
                if let Some(ref bus) = self.event_bus {
                    let _ = bus.send(AgentEvent {
                        session_id: ctx.session_key.clone(),
                        agent_id: ctx.agent_id.clone(),
                        delta: terminal_text.clone(),
                        done: true,
                        files: tool_files.clone(),
                        images: tool_images.clone(),
                        tool_log: tool_log.clone(),
                        question: None,
                    });
                }
                return Ok(AgentReply {
                    text: terminal_text,
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    needs_outer_done_emit: false,
                    outcome: crate::agent::registry::ReplyOutcome::Ok,
                });
            }
            if iteration > max_iterations {
                warn!(
                    session = %ctx.session_key,
                    iterations = iteration,
                    "agent_loop: hit max iteration limit, breaking out"
                );
                let terminal_text =
                    crate::i18n::t("agent_max_iterations", crate::i18n::default_lang()).to_owned();
                // Emit a done=true event so WS subscribers get both the
                // terminal text and the terminator frame. Without this, the
                // UI hangs waiting for done and never shows this message.
                if let Some(ref bus) = self.event_bus {
                    let _ = bus.send(AgentEvent {
                        session_id: ctx.session_key.clone(),
                        agent_id: ctx.agent_id.clone(),
                        delta: terminal_text.clone(),
                        done: true,
                        files: tool_files.clone(),
                        images: tool_images.clone(),
                        tool_log: tool_log.clone(),
                        question: None,
                    });
                }
                return Ok(AgentReply {
                    text: terminal_text,
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    needs_outer_done_emit: false,
                    outcome: crate::agent::registry::ReplyOutcome::Ok,
                });
            }
            // Check consecutive tool errors — stop early when tools keep failing.
            if error_streak >= MAX_ERROR_STREAK {
                warn!(
                    session = %ctx.session_key,
                    error_streak,
                    "agent_loop: consecutive tool errors, breaking loop"
                );
                // Return last error info to user with details
                let error_text = if let Some(ref info) = last_error_info {
                    format!("工具执行连续失败。\n\n最后错误详情：\n{}", info)
                } else {
                    crate::i18n::t("agent_tool_errors", crate::i18n::default_lang()).to_owned()
                };
                // Emit done=true so WS subscribers (desktop chat) see the
                // terminal text and the terminator frame together. Without
                // this, the UI hangs forever waiting for done — same fix
                // pattern as the clear_signal / abort / max_iterations paths.
                if let Some(ref bus) = self.event_bus {
                    let _ = bus.send(AgentEvent {
                        session_id: ctx.session_key.clone(),
                        agent_id: ctx.agent_id.clone(),
                        delta: error_text.clone(),
                        done: true,
                        files: tool_files.clone(),
                        images: tool_images.clone(),
                        tool_log: tool_log.clone(),
                        question: None,
                    });
                }
                return Ok(AgentReply {
                    text: error_text,
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: tool_files,
                    pending_analysis: None,
                    needs_outer_done_emit: false,
                    outcome: crate::agent::registry::ReplyOutcome::Ok,
                });
            }
            // Apply legacy context pruning (hard clear / soft trim) as fallback.
            if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                apply_context_pruning(sess, pruning_cfg.as_ref());
            }

            // Apply context-budget-aware trimming: trim oldest messages so the
            // persistent session fits within the context window.  The scratchpad
            // (current-turn working buffer) is NOT trimmed but its token cost is
            // subtracted from the available budget so session is trimmed enough.
            //
            // kvCacheMode >= 2 (rsclaw-server's stateful incremental protocol)
            // owns the cache server-side. Client-side trimming would delete
            // msgs the server still has cached, invalidating the prefix and
            // forcing a cold recompute on the next turn (observed in
            // production as a 75s turn after a 27K-token threshold trip).
            // Defer to the server in that mode; it splices locally without
            // breaking incremental cache hits.
            //
            // Mirror the auto-force at line ~4277: when the resolved
            // provider is `rsclaw`, the provider IS the kvCacheMode=2
            // protocol, so treat it as mode 2 even if agents.defaults
            // didn't set kv_cache_mode (default is 1).
            let configured_kv_mode = self
                .live
                .agents
                .read()
                .await
                .defaults
                .kv_cache_mode
                .unwrap_or(1);
            let (resolved_provider, _) = self.providers.resolve_model(model);
            let effective_kv_mode = if resolved_provider == "rsclaw" {
                2
            } else {
                configured_kv_mode
            };
            let scratchpad_tokens: usize = turn_scratchpad.iter().map(msg_tokens).sum();
            if effective_kv_mode < 2
                && let Some(sess) = self.sessions.get_mut(&ctx.session_key)
            {
                apply_context_budget_trim(
                    sess,
                    context_tokens,
                    &system_prompt,
                    &tools,
                    scratchpad_tokens,
                );
            }

            // Build API copy of messages for this LLM call.
            //
            // Final message order (stable prefix → volatile tail):
            //   [0]   system  — main prompt   (KV-cache anchor; contains
            //                                  ## Installed Plugins +
            //                                  ## Installed Skills via
            //                                  build_user_system — see
            //                                  prompt_builder.rs)
            //   [1…n] history — session user/assistant messages
            //   [tail] …      — turn_scratchpad  (per-iteration tools)
            let mut messages = {
                let mut raw = self
                    .sessions
                    .get(&ctx.session_key)
                    .cloned()
                    .unwrap_or_default();

                // For vision models: replace last user message with multimodal
                // version containing original images (only for this API call).
                // Must happen before scratchpad is appended so last() is the
                // session user message, not a tool result.
                if ctx.has_images {
                    if let Some(last) = raw.last_mut() {
                        if last.role == Role::User {
                            *last = ctx.user_msg_with_images.clone().unwrap_or(last.clone());
                        }
                    }
                }

                // Append current-turn scratch-paper (tool calls + results).
                // Always at the tail; discarded when this turn ends.
                raw.extend(turn_scratchpad.clone());

                // Repair transcript: ensure all tool_calls have matching tool_results.
                let repair_result = repair_tool_result_pairing(raw);

                // Synthetic tool results (generated by repair to fix broken
                // pairs) go into the scratch-paper buffer, not the persistent
                // session.  They are working-turn artefacts; no need to persist.
                if !repair_result.synthetic_messages.is_empty() {
                    turn_scratchpad.extend(repair_result.synthetic_messages.clone());
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
                    .and_then(|m| m.thinking.as_ref())
                    .cloned();
                // Clone the live default so we don't hold the lock across the
                // closure / `.and_then` chain below.
                let default_thinking = self.live.agents.read().await.defaults.thinking.clone();
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
            let tools_tokens: usize = tools
                .iter()
                .map(|t| {
                    estimate_tokens(&t.name)
                        + estimate_tokens(&t.description)
                        + estimate_tokens(&t.parameters.to_string())
                })
                .sum();
            // Use the larger of: component-based estimate vs JSON body estimate.
            // Component estimate misses JSON structure overhead, message formatting,
            // and chat template tokens. JSON body estimate (bytes / 3) is conservative
            // but never underestimates.
            let component_est = msg_tokens_sum + sys_tokens + tools_tokens;
            let body_est = {
                let msgs_json = serde_json::to_string(&messages).unwrap_or_default();
                let tools_json = serde_json::to_string(&tools).unwrap_or_default();
                // Use estimate_tokens on the full JSON body — handles CJK vs ASCII correctly.
                // Add per-message overhead for chat template tokens (~10 per message).
                estimate_tokens(&msgs_json)
                    + estimate_tokens(system_prompt)
                    + estimate_tokens(&tools_json)
                    + msg_count * 10
            };
            let approx_tokens = component_est.max(body_est);
            self.handle.update_session_tokens(
                &ctx.session_key,
                crate::agent::registry::SessionTokens {
                    sys: sys_tokens,
                    tools: tools_tokens,
                    msgs: msg_tokens_sum,
                    total: approx_tokens,
                },
            );
            info!(session = %ctx.session_key, msg_count, approx_tokens, sys_tokens, tools_tokens, msg_tokens = msg_tokens_sum, model = %model, "LLM call: context size");

            // Context usage awareness: inject hint into the LAST user message
            // (not system prompt) to preserve KV cache prefix stability.
            if approx_tokens > 0 && context_tokens > 0 {
                let usage_pct = (approx_tokens * 100) / context_tokens;
                let usage_hint = if usage_pct >= 90 {
                    Some(format!(
                        "[Context usage: {usage_pct}% — CRITICAL. \
                        Keep responses very concise. Do not re-read files already in context. \
                        Suggest user start a new session if task is complete.]"
                    ))
                } else if usage_pct >= 70 {
                    Some(format!(
                        "[Context usage: {usage_pct}%. \
                        Optimize: keep tool outputs short (use offset/limit for reads, \
                        pipe to head/tail for commands). Avoid re-reading files already in context.]"
                    ))
                } else {
                    None
                };
                if let Some(hint) = usage_hint {
                    if let Some(last_user) =
                        messages.iter_mut().rev().find(|m| m.role == Role::User)
                    {
                        match &mut last_user.content {
                            MessageContent::Text(t) => {
                                t.push_str(&format!("\n\n{hint}"));
                            }
                            MessageContent::Parts(parts) => {
                                parts.push(ContentPart::Text {
                                    text: format!("\n\n{hint}"),
                                });
                            }
                        }
                    }
                }
            }
            let effective_system = system_prompt.to_owned();
            // Per-session "## Active Plugin Tools" block is GONE as of v1.9
            // — those tools are now real ToolDefs in
            // `dynamic_prefix.user_tools` (assembled in the
            // `select_user_tools_pure` call upstream during `req.tools`
            // construction). user_tools sits in its own segment of the
            // worker prefix cache that doesn't dirty the base prefix, so
            // we don't need to push schemas into the (still-cacheable)
            // user_system text to keep cross-client cache sharing. The
            // long tail (non-headline, non-pinned tools — 200+ for
            // douyin) remains accessible via the `plugin_invoke`
            // meta-tool with zero standing prompt cost.

            // Resolve max_tokens with priority: config > built-in defaults > 0.
            // Sentinel semantics: 0 (or unset) means "no client-side cap" — we
            // omit max_tokens on the wire and let the server apply its own
            // model/tier ceiling. Only a positive value is sent. This is the
            // single normalization point, so every provider downstream receives
            // either None (omitted) or a positive number, never 0.
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

                // 4. Built-in catalog (src/provider/model_defaults.rs) — sane per-model
                //    defaults so a fresh install doesn't inherit whatever the upstream server
                //    picks (doubao = 4k → truncates mid-write_file). Only applied when none of
                //    the explicit config layers set anything; user overrides still win.
                resolve_request_max_tokens(
                    from_agent,
                    from_defaults,
                    from_provider,
                    provider_name,
                    model_id,
                )
            };

            if let Some(configured) = configured_max_tokens {
                info!(
                    session = %ctx.session_key,
                    model = %model,
                    max_tokens = configured,
                    "LLM request max_tokens"
                );
            }

            // Resolve temperature + context_limit under one read guard.
            // Both blocks consult the same `agents_live` snapshot
            // (per-agent overrides + global defaults) and run
            // back-to-back with no await in between, so a single
            // acquisition is strictly correct and halves RwLock
            // traffic on the hot path.
            //
            // Temperature resolution order (read live so hot-reload takes
            // effect on the next turn without a restart):
            //   1. Per-agent override (live.agents.list[id].temperature)
            //   2. Global defaults (live.agents.defaults.temperature)
            //   3. "Auto" heuristic — 0.6 with tools, 0.7 chat, None for thinking
            //
            // context_limit chain (matches AgentHandle.context_window so
            // /status and the pre-flight emergency compact agree):
            // per-agent model.context_tokens → defaults.context_tokens
            // → 64000. Previously read defaults only, so a per-agent
            // override of 200_000 was ignored here and emergency
            // compaction kicked in too early.
            let (temperature, context_limit) = {
                let agents_live = self.live.agents.read().await;
                let per_agent_entry = agents_live.list.iter().find(|a| a.id == self.handle.id);
                let per_agent_temp = per_agent_entry.and_then(|a| a.temperature);
                let temperature = per_agent_temp
                    .or(agents_live.defaults.temperature)
                    .map(Some)
                    .unwrap_or_else(|| {
                        if thinking_budget.is_some() {
                            None
                        } else if tools.is_empty() {
                            Some(0.7)
                        } else {
                            Some(0.6)
                        }
                    });
                let per_agent_ctx =
                    per_agent_entry.and_then(|a| a.model.as_ref().and_then(|m| m.context_tokens));
                let context_limit = per_agent_ctx
                    .or(agents_live.defaults.context_tokens)
                    .unwrap_or(64_000) as usize;
                (temperature, context_limit)
            };

            // Pre-flight check: emergency compact if we'd exceed context.
            let overhead = self.estimate_fixed_overhead();
            let session_tokens: usize = self
                .sessions
                .get(&ctx.session_key)
                .map(|msgs| msgs.iter().map(super::context_mgr::msg_tokens).sum())
                .unwrap_or(0);
            let total_est = overhead + session_tokens;
            // Use 80% of context limit as threshold to account for token estimation
            // inaccuracy (estimate is ~char/3.5, actual tokenization may differ by 10-15%).
            if total_est > (context_limit * 80 / 100) {
                warn!(
                    session = %ctx.session_key,
                    total_est,
                    context_limit,
                    overhead,
                    session_tokens,
                    "pre-flight: approaching context limit, forcing compaction"
                );
                self.compact_inner(&ctx.session_key, model, true).await;
                // Re-read messages after compaction.
                messages = self
                    .sessions
                    .get(&ctx.session_key)
                    .cloned()
                    .unwrap_or_default();
            }

            // Single live-config read per LLM iteration. Previously this
            // call site held two independent `self.live.agents.read().await`
            // acquisitions (kv_cache_mode + frequency_penalty) — minor
            // contention on the hot path, and a refactor hazard if more
            // defaults migrate in. Pull every default this iteration
            // needs at once; drop the guard before constructing the
            // request so the LLM call doesn't hold the lock.
            let (mut kv_cache_mode, frequency_penalty) = {
                let agents = self.live.agents.read().await;
                (
                    agents.defaults.kv_cache_mode.unwrap_or(1),
                    agents.defaults.frequency_penalty,
                )
            };
            // rsclaw provider only handles kv_cache_mode=2 — force it
            // when this turn's resolved provider is rsclaw, regardless
            // of agents.defaults.kv_cache_mode. The provider IS the
            // mode-2 protocol implementation, so routing-to-rsclaw is
            // itself the opt-in: no per-agent override needed.
            let (resolved_provider, _) = self.providers.resolve_model(&model);
            if resolved_provider == "rsclaw" {
                kv_cache_mode = 2;
            }
            // For kvCacheMode=2 expose the shared/user split so the rsclaw
            // provider can populate `dynamic_prefix.system` (cacheable across
            // every client of this RsClaw version) separately from
            // `dynamic_prefix.user_system` (per-client). Only the rsclaw
            // provider reads these; openai/anthropic ignore them. Internal
            // sessions use a minimal prompt that doesn't follow the
            // shared-prefix layout — leave the split unset for those (the
            // provider falls back to `system` as a single blob, with no
            // cross-client cache reuse, which matches today's behaviour).
            let (system_shared, user_system) =
                if kv_cache_mode >= 2 && !is_minimal_context_session(&ctx.session_key) {
                    let shared = crate::agent::prompt_builder::build_shared_system_prefix();
                    if let Some(rest) = effective_system.strip_prefix(&shared) {
                        let user = rest.trim_start_matches("\n\n").to_owned();
                        (Some(shared), Some(user))
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                };
            // Populate trace metadata on first iteration so SFT export
            // carries model + system_prompt + tools_schema. These don't
            // change within a turn, so init-once is correct. Without this
            // patch, `init_full_trace` left them as empty strings/`[]`,
            // producing ShareGPT JSONL with `tools: []` and no `system`
            // entry — unreplayable for training. R2 review C3.
            if let Some(ft) = ctx.full_trace.as_mut() {
                if ft.model.is_empty() {
                    ft.model = model.to_owned();
                    ft.system_prompt = effective_system.clone();
                    ft.tools_schema =
                        serde_json::to_value(&tools).unwrap_or_else(|_| serde_json::json!([]));
                }
            }
            let turn_recall = if messages
                .last()
                .is_some_and(|m| matches!(m.role, Role::User))
            {
                ctx.auto_recall.clone()
            } else {
                None
            };

            let req = LlmRequest {
                fallback_models: primary_chain_tail.clone(),
                model: model.to_owned(),
                messages,
                tools: tools.clone(),
                system: Some(effective_system.clone()),
                max_tokens: configured_max_tokens,
                temperature,
                frequency_penalty,
                thinking_budget,
                endpoint: Default::default(),
                kv_cache_mode,
                session_key: if kv_cache_mode >= 2 {
                    Some(ctx.session_key.clone())
                } else {
                    None
                },
                system_shared,
                user_system,
                recall: turn_recall,
            };

            // Update live status: LLM call starting.
            if let Ok(mut status) = self.live_status.try_write() {
                status.state = "streaming".to_owned();
            }

            let providers = Arc::clone(&self.providers);
            let stream_result = self.failover.call(req.clone(), &providers).await;

            // If the LLM rejects for context overflow, compact and retry once.
            let mut stream = match stream_result {
                Err(ref e)
                    if e.to_string().contains("exceed") || e.to_string().contains("context") =>
                {
                    warn!(session = %ctx.session_key, error = %e, "LLM context overflow, compacting and retrying");
                    self.compact_inner(&ctx.session_key, &model, true).await;
                    // Rebuild messages after compaction.
                    let compacted = self
                        .sessions
                        .get(&ctx.session_key)
                        .cloned()
                        .unwrap_or_default();
                    let mut retry_req = req.clone();
                    retry_req.messages = compacted;
                    self.failover.call(retry_req, &providers).await?
                }
                other => other?,
            };
            let mut text_buf = String::new();
            let mut reasoning_buf = String::new();
            let mut tool_calls: Vec<(String, String, Value)> = Vec::new();
            // Track loop detection warnings per tool call id (to inject into result)
            let mut loop_warnings: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            // Streaming throttle: batch small deltas to reduce channel update rate.
            // The 150ms cadence exists to spare Feishu/DingTalk interactive
            // cards (in-place edits are rate-limited). The desktop UI streams
            // over a local WebSocket with no such limit, so the same 150ms
            // throttle there chops a token-at-a-time model (e.g. deepseek emits
            // one char per SSE frame) into visible ~7fps stutter. Give the
            // local desktop path a tighter 100ms cadence — noticeably smoother
            // than 150ms without flooding the frontend's per-delta markdown
            // re-parse (which has no render throttle of its own). Card-based
            // channels keep 150ms.
            let delta_flush_interval = if matches!(ctx.channel.as_str(), "ws" | "desktop") {
                std::time::Duration::from_millis(100)
            } else {
                std::time::Duration::from_millis(150)
            };
            let mut delta_buf = String::new();
            let mut last_delta_flush = std::time::Instant::now();

            loop {
                // Idle watchdog: bound each await on the next stream event so a
                // stalled connection (200 OK then silence) can't wedge the
                // worker queue. A healthy stream — even a slow reasoning model —
                // emits deltas well within this window.
                let event = match tokio::time::timeout(
                    Duration::from_secs(STREAM_IDLE_TIMEOUT_SECS),
                    stream.next(),
                )
                .await
                {
                    Ok(Some(ev)) => ev,
                    Ok(None) => break,
                    Err(_) => {
                        return Err(anyhow!(
                            "LLM stream stalled: no data for {STREAM_IDLE_TIMEOUT_SECS}s"
                        ));
                    }
                };
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
                        if delta_buf.len() >= 80 || elapsed >= delta_flush_interval {
                            if let Some(ref bus) = self.event_bus {
                                let _ = bus.send(AgentEvent {
                                    session_id: ctx.session_key.clone(),
                                    agent_id: ctx.agent_id.clone(),
                                    delta: std::mem::take(&mut delta_buf),
                                    done: false,
                                    files: vec![],
                                    images: vec![],
                                    tool_log: vec![],
                                    question: None,
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
                            // Use check_with_params which hashes the full input
                            // (OpenClaw-compatible). This ensures
                            // different arguments count as different calls.
                            if let Some(warning_msg) = ctx
                                .loop_detector
                                .check_with_params(&name, &input)
                                .to_result()?
                            {
                                tracing::warn!(tool = %name, params = ?input, "{}", warning_msg);
                                // Store warning to inject into tool result (so LLM sees it)
                                loop_warnings.insert(id.clone(), warning_msg.clone());
                                ctx.loop_warning_triggered = true;
                                // Factual trace for end-of-turn failure-lesson
                                // extraction (ground truth: what was actually
                                // called, truncated). First loop of the turn wins.
                                if ctx.loop_failure.is_none() {
                                    let args = serde_json::to_string(&input).unwrap_or_default();
                                    let args: String = args.chars().take(400).collect();
                                    ctx.loop_failure =
                                        Some(format!("tool={name}; args={args}; {warning_msg}"));
                                }
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
                                    last.2 =
                                        serde_json::Value::String(format!("{existing}{fragment}"));
                                } else if let Some(new_str) = input.as_str() {
                                    // Last is Object but new chunk is String — convert.
                                    let existing_str =
                                        serde_json::to_string(&last.2).unwrap_or_default();
                                    last.2 = serde_json::Value::String(format!(
                                        "{existing_str}{new_str}"
                                    ));
                                } else {
                                    // Last resort: convert both to string.
                                    let existing_str =
                                        serde_json::to_string(&last.2).unwrap_or_default();
                                    let fragment =
                                        serde_json::to_string(&input).unwrap_or_default();
                                    last.2 = serde_json::Value::String(format!(
                                        "{existing_str}{fragment}"
                                    ));
                                    tracing::debug!(
                                        "streaming tool call: merged non-string types as strings"
                                    );
                                }
                            }
                        }
                    }
                    StreamEvent::Done { usage } => {
                        // Update context total with real usage from LLM if available.
                        if let Some(ref u) = usage {
                            let real_tokens = (u.input + u.output) as usize;
                            if let Ok(mut map) = self.handle.session_tokens.write() {
                                if let Some(st) = map.get_mut(&ctx.session_key) {
                                    st.total = real_tokens;
                                }
                            }
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
                        files: vec![],
                        images: vec![],
                        tool_log: vec![],
                        question: None,
                    });
                }
            }

            // Strip <think>...</think> tags from accumulated text.
            // Auto-enabled when thinking is not explicitly requested (budget=0 or None),
            // since some models (MiniMax, QwQ) may still emit <think> tags regardless.
            // Can be overridden via agents.defaults.stripThinkTags.
            let pre_strip_len = text_buf.trim().len();
            let thinking_active = thinking_budget.unwrap_or(0) > 0;
            let strip_enabled = self
                .live
                .agents
                .read()
                .await
                .defaults
                .strip_think_tags
                .unwrap_or(!thinking_active);
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
            //
            // IMPORTANT: only fall back when there are NO tool calls. When the model
            // emits `reasoning + tool_calls` (qwen-thinking, claude-extended-thinking,
            // etc. doing silent tool use), promoting reasoning_buf into text_buf leaks
            // the chain-of-thought through the intermediate-output path
            // (`notification_tx` below) and the user sees CoT text bubbles like
            // "用户现在需要..." / "对，先执行第一步...".
            tracing::info!(
                text_len = text_buf.len(),
                reasoning_len = reasoning_buf.len(),
                tool_call_count = tool_calls.len(),
                "agent_loop: post-stream buffers"
            );
            if text_buf.trim().is_empty()
                && !reasoning_buf.trim().is_empty()
                && tool_calls.is_empty()
            {
                tracing::info!(
                    reasoning_len = reasoning_buf.len(),
                    "agent_loop: using reasoning as reply text"
                );
                text_buf = reasoning_buf.clone();
            }

            // Capture per-iteration thinking into the trace so SFT data
            // preserves the <think>...</think> reasoning content that
            // preceded the tool call (or final reply) this round. Push
            // once per iteration, only when reasoning content exists.
            // R2 review C3 — `push_thinking` had zero production call
            // sites before this; reasoning was invisible to SFT export.
            if let Some(ft) = ctx.full_trace.as_mut() {
                if !reasoning_buf.trim().is_empty() {
                    ft.push_thinking(reasoning_buf.clone());
                }
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

            // Restore provider-encoded tool names *after* all SSE
            // fragments are accumulated. OpenAI rejects function names
            // outside `^[a-zA-Z0-9_-]+$`, so plugin tools like
            // `wechat.send_text` are wire-encoded to `rc_wechat_d_...`
            // before the request; we reverse the encoding here, when
            // the full name is finally known. `restore_tool_name` is
            // idempotent for non-`rc_` names so it's safe to apply
            // regardless of which provider produced the chunks.
            for (_, name, _) in tool_calls.iter_mut() {
                *name = crate::provider::openai::restore_tool_name(name);
            }

            // Rescue tool calls from text output — some small models (qwen3.5:9b)
            // emit tool calls as XML text instead of proper function_call format.
            // Detect <tool_call>/<function=...> patterns and parse them.
            if tool_calls.is_empty() && text_buf.contains("<function=") {
                // Parameter values are coerced to their schema-declared type
                // (so e.g. ask_user.options stays an array, not a string).
                let rescued =
                    crate::agent::tool_call_repair::rescue_tool_calls_from_text(&text_buf, &tools);
                for (id, name, input) in rescued {
                    // WARN, not INFO: a text tool call means the inference server
                    // did not return native `tool_calls` for this model/node.
                    // The rescue keeps us working, but this is a fleet-config
                    // signal (llama.cpp tool-call template/grammar) — alert on
                    // `rescued_from_text` to find nodes still emitting text.
                    tracing::warn!(
                        tool = %name,
                        rescued_from_text = true,
                        params = ?input.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                        "agent_loop: rescued tool call from <function=> text — server returned no native tool_calls"
                    );
                    tool_calls.push((id, name, input));
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
                // Deception detection: model claims action but no tool was called.
                // This is a critical trust violation that must be flagged to the user.
                // IMPORTANT: Check turn_scratchpad for tool calls from earlier iterations,
                // not just current iteration's tool_calls (which is empty at this point).
                let deception_keywords = [
                    "已委托",
                    "已用opencode",
                    "已让opencode",
                    "委托给opencode",
                    "已检查",
                    "已搜索",
                    "已运行",
                    "已执行",
                    "已交给",
                    "交给opencode",
                    "opencode正在",
                    "opencode已经",
                    "I delegated",
                    "I asked opencode",
                    "opencode is",
                    "I ran",
                    "I checked",
                    "I searched",
                    "I executed",
                ];
                let lower_text = text_buf.to_lowercase();
                let claims_action = deception_keywords
                    .iter()
                    .any(|kw| lower_text.contains(&kw.to_lowercase()) || text_buf.contains(kw));

                // Check if turn_scratchpad contains tool calls (from earlier iterations)
                let has_tool_in_turn = turn_scratchpad.iter().any(|msg| {
                    if let crate::provider::MessageContent::Parts(parts) = &msg.content {
                        parts.iter().any(|p| {
                            matches!(p, crate::provider::ContentPart::ToolUse { name, .. }
                                if name == "opencode" || name == "claudecode" || name == "codex"
                                    || name == "web_search" || name == "shell" || name == "execute_command")
                        })
                    } else {
                        false
                    }
                });

                // Only flag deception if model claims action AND no tool was called in entire
                // turn
                if claims_action && !text_buf.trim().is_empty() && !has_tool_in_turn {
                    tracing::warn!(
                        session = %ctx.session_key,
                        text_preview = %text_buf.chars().take(200).collect::<String>(),
                        has_tool_in_turn = has_tool_in_turn,
                        "DECEPTION DETECTED: model claims action but no tool_call in turn"
                    );
                    // Send warning via notification channel (streaming already sent original text).
                    // Append warning to text_buf so it's persisted in session history.
                    let warning = "\n\n⚠️ **警告**: 模型声称已执行操作但实际上没有调用任何工具。\
                        这是欺骗行为。请回复「重试并实际调用工具」强制模型执行。";
                    text_buf.push_str(warning);
                    // Also send immediately via notification so user sees it.
                    if let Some(ref ntx) = self.notification_tx {
                        let notif_target = if !ctx.chat_id.is_empty() {
                            ctx.chat_id.clone()
                        } else {
                            ctx.peer_id.clone()
                        };
                        let _ = ntx.send(crate::channel::OutboundMessage {
                            target_id: notif_target,
                            is_group: false,
                            text: "⚠️ **欺骗警告**: 模型声称「已委托/已检查」但没有调用任何工具。\
                                这是欺骗行为。\n\n请回复「重试」强制模型实际调用 opencode 工具。"
                                .to_owned(),
                            reply_to: None,
                            images: vec![],
                            files: vec![],
                            channel: Some(ctx.channel.clone()),
                            account: None,
                        });
                    }
                }

                // Only persist non-empty assistant replies to session.
                // Empty responses pollute history and confuse the LLM on
                // subsequent turns (it sees its own empty reply and mimics it).
                if !text_buf.trim().is_empty() {
                    let assistant_msg = Message {
                        role: Role::Assistant,
                        content: MessageContent::Text(text_buf.clone()),
                        rsclaw_hidden: None,
                    };
                    // Internal sessions (heartbeat/cron/system): skip DB
                    // persist so replies like "HEARTBEAT_OK" don't
                    // accumulate in session history.
                    if !is_internal_session(&ctx.session_key) {
                        if let Err(e) = self.store.db.append_message(
                            &ctx.session_key,
                            &serde_json::to_value(&assistant_msg).unwrap_or_default(),
                        ) {
                            tracing::error!(error = %e, "failed to persist message");
                        }
                        if let Some(sess) = self.sessions.get_mut(&ctx.session_key) {
                            sess.push(assistant_msg);
                        }
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
                        files: tool_files.clone(),
                        images: tool_images.clone(),
                        tool_log: tool_log.clone(),
                        question: None,
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

                // Workflow crystallization trigger — only on the normal
                // (non-error_streak / non-max-iter / non-abort) completion
                // path so we never persist failed workflows as skills.
                ctx.turn_metrics.final_text_len = final_text.len();
                if let Some(ft) = ctx.full_trace.as_mut() {
                    if !final_text.is_empty() {
                        ft.push_assistant_text(&final_text);
                    }
                }
                if let Some(ft) = ctx.full_trace.take() {
                    maybe_emit_trace(ft);
                }
                self.maybe_crystallize_workflow(&ctx, &final_text);

                if !tool_images.is_empty() {
                    info!(
                        "AgentReply returning with {} image(s), first {} bytes",
                        tool_images.len(),
                        tool_images.first().map(|s| s.len()).unwrap_or(0)
                    );
                }
                // tool_images may contain file paths (from computer_use
                // screenshots saved to disk) OR data URLs (from image-gen
                // tools).  The event_bus already emitted the unchanged values
                // for the WS/desktop client, which loads file paths via
                // Tauri's asset protocol.  Non-WS channels (telegram, feishu,
                // wechat, ...) only look at AgentReply.images and expect the
                // `data:image/...;base64,...` format, so rehydrate any file
                // paths here before returning.
                let reply_images = tool_images
                    .into_iter()
                    .filter_map(|i| image_ref_to_data_url(i))
                    .collect::<Vec<_>>();
                return Ok(AgentReply {
                    text: final_text,
                    is_empty: no_reply && reply_images.is_empty(),
                    tool_calls: None,
                    images: reply_images,
                    files: tool_files,
                    pending_analysis: None,
                    needs_outer_done_emit: false,
                    outcome: crate::agent::registry::ReplyOutcome::Ok,
                });
            }

            // Send intermediate text to user immediately (progress feedback).
            // Model often says "好的，我来帮你搜索" before calling tools — send it now
            // instead of waiting for the entire turn to complete.
            //
            // SKIP for ws/desktop channels: WS clients already receive
            // streaming `delta` events through the event_bus pipeline that
            // render progressively into the main reply bubble. Sending the
            // same text via notification_tx would surface as a duplicate
            // standalone bubble (the "ws" alias in startup.rs bridges
            // notification_tx to the desktop channel, so it lands in chat
            // alongside the streaming bubble).
            let is_streaming_channel = ctx.channel == "ws" || ctx.channel == "desktop";
            let intermediate_enabled = self
                .live
                .agents
                .read()
                .await
                .defaults
                .intermediate_output
                .unwrap_or(true);
            if intermediate_enabled
                && !is_streaming_channel
                && !tool_calls.is_empty()
                && let Some(intermediate_text) = intermediate_notification_text(&text_buf)
            {
                if let Some(ref ntx) = self.notification_tx {
                    let notif_target = if !ctx.chat_id.is_empty() {
                        ctx.chat_id.clone()
                    } else {
                        ctx.peer_id.clone()
                    };
                    let _ = ntx.send(crate::channel::OutboundMessage {
                        target_id: notif_target,
                        is_group: false,
                        text: intermediate_text.to_owned(),
                        reply_to: None,
                        images: vec![],
                        files: vec![],
                        channel: Some(ctx.channel.clone()),
                        account: None,
                    });
                    tracing::debug!(
                        text_len = intermediate_text.len(),
                        "agent_loop: sent intermediate text to user"
                    );
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
            // Persist reasoning_content so providers that require it (e.g.
            // kimi-for-coding) see it on subsequent turns.
            if !reasoning_buf.is_empty() {
                parts.push(crate::provider::ContentPart::Reasoning {
                    text: reasoning_buf,
                });
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
                rsclaw_hidden: None,
            };
            // Tool-use responses are scratch-paper: the LLM needs to see them
            // in this turn's messages, but they must not persist in session history.
            turn_scratchpad.push(assistant_msg);

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
                    needs_outer_done_emit: false,
                    outcome: crate::agent::registry::ReplyOutcome::Ok,
                });
            }

            // Three-phase tool dispatch:
            //   Phase 1 (serial): preflight — parse-error skip, loop
            //     detection, last_tool_key bookkeeping, max_iterations
            //     upgrade. Anything that may early-return BEFORE side
            //     effects must happen here.
            //   Phase 2a (serial): pre-dispatch side effects — live_status,
            //     before_tool_call hook, A2A progress emit, cancel check.
            //     Deferred from Phase 1 so a Phase-1 early-return doesn't
            //     leave dangling before_tool_call hooks without a matching
            //     after_tool_call.
            //   Phase 2b (parallel): join_all(dispatch_tool) with per-tool
            //     timeout — one stuck sub-agent / wedged HTTP call no
            //     longer blocks every other tool in the same turn.
            //   Phase 3 (serial, in LLM emit order): after_tool_call hook,
            //     metrics, full_trace, loop_detector, image/file extraction,
            //     event-bus emit, session compression, turn_scratchpad push.
            struct PendingDispatch {
                tool_id: String,
                tool_name: String,
                tool_input: Value,
                tool_input_str: String,
            }
            let mut to_dispatch: Vec<PendingDispatch> = Vec::new();

            // ---- Phase 1: serial preflight ----
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
                        ctx.loop_detector
                            .record_result(&serde_json::json!({"error": "too many parse errors"}));
                        // Return error to break the loop
                        return Err(anyhow!(
                            "Turn aborted: {} consecutive tool parse errors. Model output may be corrupted.",
                            ctx.parse_error_count
                        ));
                    }

                    // Record for loop detection so error doesn't count as a "different result"
                    ctx.loop_detector
                        .record_result(&serde_json::json!({"error": err_msg}));

                    // Directly return error to scratch-paper buffer without executing the tool.
                    let tool_msg = Message {
                        role: Role::Tool,
                        content: MessageContent::Parts(vec![
                            crate::provider::ContentPart::ToolResult {
                                tool_use_id: tool_id.clone(),
                                content: format!(r#"{{"error":"{}"}}"#, err_msg),
                                is_error: Some(true),
                            },
                        ]),
                        rsclaw_hidden: None,
                    };
                    turn_scratchpad.push(tool_msg);
                    continue;
                }

                // Detect consecutive identical tool calls (same name + same args).
                let call_key =
                    crate::agent::loop_detection::hash_tool_call(&tool_name, &tool_input);
                if call_key == last_tool_key {
                    same_call_streak += 1;
                    ctx.turn_metrics.same_call_streak_max =
                        ctx.turn_metrics.same_call_streak_max.max(same_call_streak);
                    if same_call_streak >= MAX_SAME_CALL_STREAK {
                        warn!(
                            tool = %tool_name,
                            streak = same_call_streak,
                            "agent_loop: identical tool call repeated {} times, breaking loop",
                            same_call_streak
                        );
                        let terminal_text =
                            crate::i18n::t("agent_loop_detected", crate::i18n::default_lang())
                                .to_owned();
                        // Emit done=true so WS subscribers (desktop chat) see
                        // the terminal text and the terminator frame together.
                        // Same fix pattern as the clear_signal / abort /
                        // max_iterations / error_streak paths.
                        if let Some(ref bus) = self.event_bus {
                            let _ = bus.send(AgentEvent {
                                session_id: ctx.session_key.clone(),
                                agent_id: ctx.agent_id.clone(),
                                delta: terminal_text.clone(),
                                done: true,
                                files: tool_files.clone(),
                                images: tool_images.clone(),
                                tool_log: tool_log.clone(),
                                question: None,
                            });
                        }
                        return Ok(AgentReply {
                            text: terminal_text,
                            is_empty: false,
                            tool_calls: None,
                            images: vec![],
                            files: vec![],
                            pending_analysis: None,
                            needs_outer_done_emit: false,
                            outcome: crate::agent::registry::ReplyOutcome::Ok,
                        });
                    }
                } else {
                    last_tool_key = call_key;
                    same_call_streak = 1;
                }

                // Upgrade iteration limit when complex or multi-step tools are used.
                if matches!(
                    tool_name.as_str(),
                    "web_browser"
                        | "opencode"
                        | "claudecode"
                        | "agent"
                        | "search_content"
                        | "search_file"
                        | "shell"
                        | "execute_command"
                        | "exec"
                ) {
                    max_iterations = max_iterations.max(configured_complex);
                }

                let tool_input_str = tool_input.to_string();
                to_dispatch.push(PendingDispatch {
                    tool_id,
                    tool_name,
                    tool_input,
                    tool_input_str,
                });
            }

            // ---- Phase 2a: serial pre-dispatch side effects ----
            for p in &to_dispatch {
                info!(tool = %p.tool_name, "dispatching tool call");
                if let Ok(mut status) = self.live_status.try_write() {
                    status.state = "tool_call".to_owned();
                    status.tool_history.push(p.tool_name.clone());
                }
                self.fire_hook(
                    "before_tool_call",
                    json!({
                        "agent_id": self.handle.id,
                        "tool": p.tool_name,
                        "input": p.tool_input,
                    }),
                )
                .await;
                ctx.turn_ctx
                    .emit_working(&format!("calling tool {}", p.tool_name));
                if ctx.turn_ctx.is_cancelled() {
                    return Err(anyhow!("canceled by A2A CancelTask"));
                }
            }

            // ---- Phase 2b: parallel dispatch + real-time per-tool emit ----
            // FuturesUnordered (instead of join_all) lets the user see
            // each tool's result the instant it completes — no longer
            // batched at max(t1,t2,t3). dispatch_tool is (&self, &ctx)
            // immutable, so concurrent calls are safe. Per-tool timeout
            // caps a hung sub-agent / wedged HTTP call.
            //
            // The `<rstool>` channel emit that previously lived in Phase 3
            // now fires here inside each future's tail, so streaming
            // channel subscribers (desktop WS, etc.) get per-tool cards
            // as they finish rather than all at the slowest's latency.
            let dispatch_stream: futures::stream::FuturesUnordered<_> = to_dispatch
                .iter()
                .enumerate()
                .map(|(idx, p)| {
                    let bus = self.event_bus.clone();
                    let session_id = ctx.session_key.clone();
                    let agent_id = ctx.agent_id.clone();
                    let tool_name = p.tool_name.clone();
                    let tool_input_str = p.tool_input_str.clone();
                    let fut =
                        self.dispatch_tool(ctx, &p.tool_id, &p.tool_name, p.tool_input.clone());
                    async move {
                        let timed =
                            time::timeout(Duration::from_secs(TOOL_DISPATCH_TIMEOUT_SECS), fut)
                                .await;
                        if let Some(bus) = bus {
                            let preview = match &timed {
                                Ok(Ok(v)) => {
                                    // Render through `format_tool_result` so
                                    // shape-specific tools (exec, read,
                                    // web_search, …) emit clean text instead
                                    // of raw JSON. Then collapse any oversize
                                    // string fields (image data URLs, full
                                    // HTML, base64 blobs) so the UI card
                                    // doesn't carry raw payload —
                                    // `compact_value` runs in Phase 3 against
                                    // the same value and produces the canonical
                                    // artifact, so duplicating that write here
                                    // would burn disk; we just squash for the
                                    // streaming preview.
                                    let raw = if v.is_string() {
                                        v.as_str().unwrap_or("").to_owned()
                                    } else {
                                        let squashed = squash_large_strings(v, 1_000);
                                        format_tool_result(&squashed)
                                    };
                                    if raw.chars().count() > 4000 {
                                        let truncated: String = raw.chars().take(2000).collect();
                                        format!("{truncated}…(truncated)")
                                    } else {
                                        raw
                                    }
                                }
                                Ok(Err(e)) => format!("[error: {e:#}]"),
                                Err(_) => format!("[timeout after {TOOL_DISPATCH_TIMEOUT_SECS}s]"),
                            };
                            // Exec tools: prepend `$ command` so the UI's
                            // card header shows what was run (parity with
                            // the old Phase-3 emit).
                            let display_out = if matches!(
                                tool_name.as_str(),
                                "shell" | "execute_command" | "exec"
                            ) {
                                serde_json::from_str::<serde_json::Value>(&tool_input_str)
                                    .ok()
                                    .and_then(|a| {
                                        a.get("command")
                                            .and_then(|c| c.as_str())
                                            .map(|cmd| format!("$ {cmd}\n{preview}"))
                                    })
                                    .unwrap_or(preview)
                            } else {
                                preview
                            };
                            let marker =
                                format!("<rstool name=\"{tool_name}\">{display_out}</rstool>");
                            let _ = bus.send(AgentEvent {
                                session_id,
                                agent_id,
                                delta: marker,
                                done: false,
                                files: vec![],
                                images: vec![],
                                tool_log: vec![],
                                question: None,
                            });
                        }
                        (idx, timed)
                    }
                })
                .collect();

            // Drain in completion order so the emit above fires real-time,
            // then re-index by emit position so Phase 3 can iterate the LLM's
            // original tool_call order (matters for scratchpad / loop
            // detector / metrics determinism).
            let mut timed_results: Vec<Option<_>> = (0..to_dispatch.len()).map(|_| None).collect();
            {
                use futures::StreamExt as _;
                let mut stream = dispatch_stream;
                while let Some((idx, r)) = stream.next().await {
                    timed_results[idx] = Some(r);
                }
            }
            let timed_results: Vec<_> = timed_results
                .into_iter()
                .map(|o| o.expect("every dispatch future yields exactly one result"))
                .collect();

            // ---- Phase 3: serial post-processing in LLM emit order ----
            for (pending, timed_result) in to_dispatch.into_iter().zip(timed_results.into_iter()) {
                let PendingDispatch {
                    tool_id,
                    tool_name,
                    tool_input: tool_input_for_metrics,
                    tool_input_str,
                } = pending;

                let result: Result<Value> = match timed_result {
                    Ok(inner) => inner,
                    Err(_elapsed) => Err(anyhow!(
                        "tool '{}' timed out after {}s",
                        tool_name,
                        TOOL_DISPATCH_TIMEOUT_SECS
                    )),
                };

                self.fire_hook(
                    "after_tool_call",
                    json!({
                        "agent_id": self.handle.id,
                        "tool": tool_name,
                        "ok": result.is_ok(),
                    }),
                )
                .await;

                let (mut result_text, result_images) = match result {
                    Ok(v) => {
                        // Reset parse error counter on successful tool execution
                        ctx.parse_error_count = 0;
                        // Tool result indicates failure if any of:
                        //   exit_code != 0  |  has "error" field  |  stderr length > 0
                        let has_error = match &v {
                            serde_json::Value::Object(obj) => {
                                obj.get("exit_code")
                                    .and_then(|c| c.as_i64())
                                    .map(|c| c != 0)
                                    .unwrap_or(false)
                                    || obj.contains_key("error")
                                    || obj
                                        .get("stderr")
                                        .and_then(|s| s.as_str())
                                        .map(|s| !s.is_empty())
                                        .unwrap_or(false)
                            }
                            _ => {
                                // Fallback: check string representation
                                let v_str = v.to_string();
                                v_str.contains("\"exit_code\":")
                                    && !v_str.contains("\"exit_code\":0")
                                    && !v_str.contains("\"exit_code\": 0")
                                    || v_str.contains("\"error\"")
                                    || v_str.contains("\"stderr\":")
                                        && !v_str.contains("\"stderr\":\"\"")
                            }
                        };
                        if has_error {
                            error_streak += 1;
                            last_error_info = Some(v.to_string());
                        } else {
                            error_streak = 0;
                            last_error_info = None;
                        }
                        // Record into per-turn metrics for workflow
                        // crystallization. Truncate args/result so we don't
                        // hold megabytes of base64 screenshots in RAM.
                        const SUMMARY_CHARS: usize = 400;
                        let args_summary: String = serde_json::to_string(&tool_input_for_metrics)
                            .unwrap_or_default()
                            .chars()
                            .take(SUMMARY_CHARS)
                            .collect();
                        let result_summary: String =
                            v.to_string().chars().take(SUMMARY_CHARS).collect();
                        ctx.turn_metrics.record_tool(
                            &tool_name,
                            args_summary,
                            result_summary,
                            has_error,
                        );
                        if let Some(ft) = ctx.full_trace.as_mut() {
                            ft.push_tool_call(&tool_name, tool_input_for_metrics.clone(), &tool_id);
                            ft.push_tool_result(&tool_id, v.to_string(), has_error);
                        }
                        // Record result for progress-aware loop detection.
                        // Same args + different results = making progress, not a loop.
                        // For exec tool: exclude task_id (uuid changes each call) to properly
                        // detect loops.
                        let result_for_loop = if tool_name == "shell"
                            || tool_name == "exec"
                            || tool_name == "execute_command"
                        {
                            // Strip task_id from exec results - it's a uuid that changes every call
                            match &v {
                                serde_json::Value::Object(obj) => {
                                    let mut cleaned = serde_json::Map::new();
                                    for (k, val) in obj.iter() {
                                        if k != "task_id" {
                                            cleaned.insert(k.clone(), val.clone());
                                        }
                                    }
                                    serde_json::Value::Object(cleaned)
                                }
                                _ => v.clone(),
                            }
                        } else {
                            v.clone()
                        };
                        ctx.loop_detector.record_result(&result_for_loop);
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
                        // base64 back to LLM. Check "image" (data-URL screenshots
                        // and image-gen), "image_path" (new screenshot path), and
                        // "url" (image gen). File paths get forwarded as-is so
                        // the UI can load them via Tauri's asset protocol —
                        // much lighter than shipping base64 over WS.
                        let img_data = v.get("image").and_then(|i| i.as_str()).or_else(|| {
                            v.get("url")
                                .and_then(|u| u.as_str())
                                .filter(|u| u.starts_with("data:image/"))
                        });
                        let img_path = v.get("image_path").and_then(|p| p.as_str());
                        // computer_use screenshots are internal agent state —
                        // never auto-send to the user. Only image-gen and explicit uploads
                        // should forward images.
                        let is_internal_screenshot = tool_name == "computer_use";
                        if !is_internal_screenshot && let Some(img) = img_data.or(img_path) {
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
                        } else {
                            // Runtime backstop: every tool's output funnels through here.
                            // Oversized payloads get written to the artifact store and
                            // replaced with a head+tail preview + read_artifact hint.
                            // One enforcement point — no tool can leak a giant payload
                            // even if its handler forgot to compact.
                            //
                            // Exception: `read_artifact` itself is the recovery path —
                            // compacting its return would re-write the user's full-read
                            // request back to a new artifact and loop the LLM through
                            // increasingly nested previews.
                            // Both recovery tools (read_artifact / read_session_archive)
                            // bypass the backstop — re-compacting their full-read
                            // response would write a new artifact and force the LLM
                            // into a nested re-fetch loop.
                            let v = if matches!(
                                tool_name.as_str(),
                                "read_artifact" | "read_session_archive"
                            ) {
                                v
                            } else {
                                // Web tools fetch articles where the LLM
                                // usually wants the lede + structure in
                                // one shot; a wider preview saves a
                                // follow-up read_artifact call.
                                let budget = match tool_name.as_str() {
                                    "web_fetch" | "web_browser" | "web_search" => {
                                        crate::artifact::PreviewBudget::WEB
                                    }
                                    _ => crate::artifact::PreviewBudget::DEFAULT,
                                };
                                crate::artifact::compact_value(
                                    crate::artifact::default_store(),
                                    &ctx.session_key,
                                    v,
                                    budget,
                                )
                            };
                            if let Some(s) = v.as_str() {
                                (s.to_owned(), vec![])
                            } else {
                                // Format structured tool results (exec, read, etc.) for better LLM
                                // comprehension
                                let mut text = format_tool_result(&v);
                                // Surface the artifact envelope to the LLM —
                                // format_tool_result is shape-specific (exec
                                // returns stdout+stderr, read returns content,
                                // …) and drops envelope metadata. Append an
                                // explicit marker so the LLM sees the
                                // tool_result_id even when the preview text's
                                // inline hint sits buried mid-output.
                                if v.get("_truncated")
                                    .and_then(|x| x.as_bool())
                                    .unwrap_or(false)
                                {
                                    if let Some(id) =
                                        v.get("_tool_result_id").and_then(|x| x.as_str())
                                    {
                                        text.push_str(&format!(
                                            "\n\n[truncated — call read_artifact(tool_result_id=\"{id}\") for full output]"
                                        ));
                                    } else if let Some(ids) =
                                        v.get("_tool_result_ids").and_then(|x| x.as_object())
                                    {
                                        let pairs: Vec<String> = ids
                                            .iter()
                                            .filter_map(|(k, v)| {
                                                v.as_str().map(|s| format!("{k}={s}"))
                                            })
                                            .collect();
                                        if !pairs.is_empty() {
                                            text.push_str(&format!(
                                                "\n\n[truncated — fields compacted: {}. Call read_artifact with the id of the field you need.]",
                                                pairs.join(", ")
                                            ));
                                        }
                                    }
                                }
                                (text, vec![])
                            }
                        }
                    }
                    Err(e) => {
                        // Use {:#} (anyhow alternate Display) to include the
                        // full error chain — without this the LLM only sees
                        // the outermost with_context() wrapper and root cause
                        // (wasm trap, http status, panic msg) is hidden.
                        let err_chain = format!("{e:#}");
                        warn!(tool = %tool_name, "tool error: {}", err_chain);
                        // Store error info for user feedback when breaking loop
                        last_error_info = Some(err_chain.clone());
                        // Record error result for loop detection (errors count as results too).
                        ctx.loop_detector
                            .record_result(&serde_json::json!({"error": err_chain.clone()}));
                        let payload = serde_json::json!({
                            "error": err_chain,
                            "_do_not_retry": true,
                            "hint": "This tool call failed. Do NOT retry the same tool with the same arguments. Try a different approach or inform the user.",
                        });
                        (payload.to_string(), vec![])
                    }
                };

                tool_images.extend(result_images);

                // Record tool call for frontend display (truncated to 4000 chars).
                // The companion `<rstool>` channel emit moved to Phase 2b so
                // streaming subscribers see per-tool cards in real time; this
                // block only builds `tool_log` for the final AgentReply
                // (rendered by non-streaming channels like Feishu/WeChat).
                {
                    let args_str = tool_input_str;
                    let out_str = if result_text.len() > 4000 {
                        let truncated: String = result_text.chars().take(2000).collect();
                        format!("{}…(truncated)", truncated)
                    } else {
                        result_text.clone()
                    };
                    tool_log.push((tool_name.clone(), args_str, out_str));
                }

                // Auto-send files: any tool returning __send_file=true queues the
                // file for delivery. Images go to tool_images, others to tool_files.
                {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&result_text) {
                        if v.get("__send_file")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false)
                        {
                            if let Some(path_str) = v.get("path").and_then(|p| p.as_str()) {
                                let send_workspace = self
                                    .handle
                                    .config
                                    .workspace
                                    .as_deref()
                                    .or(self.live.agents.read().await.defaults.workspace.as_deref())
                                    .map(expand_tilde)
                                    .unwrap_or_else(|| {
                                        crate::config::loader::base_dir().join("workspace")
                                    });
                                let full = canonicalize_external_path(path_str, &send_workspace);
                                let filename = v
                                    .get("filename")
                                    .and_then(|f| f.as_str())
                                    .unwrap_or("file")
                                    .to_owned();
                                let lower = filename.to_lowercase();
                                let is_image = lower.ends_with(".jpg")
                                    || lower.ends_with(".jpeg")
                                    || lower.ends_with(".png")
                                    || lower.ends_with(".webp")
                                    || lower.ends_with(".gif");
                                if is_image {
                                    // Send the path inline. Desktop UI loads via Tauri's
                                    // asset protocol; `image_ref_to_data_url` converts to
                                    // a base64 data URL only for non-WS channels at the
                                    // AgentReply boundary.
                                    if full.exists() {
                                        tool_images.push(full.to_string_lossy().into_owned());
                                        tracing::info!(path = %full.display(), "agent: send_file queued as image (path)");
                                    } else {
                                        tracing::warn!(path = %full.display(), "agent: send_file image path missing, dropping");
                                    }
                                } else {
                                    let mime = if lower.ends_with(".xlsx") {
                                        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                                    } else if lower.ends_with(".docx") {
                                        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                                    } else if lower.ends_with(".pptx") {
                                        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                                    } else if lower.ends_with(".pdf") {
                                        "application/pdf"
                                    } else if lower.ends_with(".csv") {
                                        "text/csv"
                                    } else if lower.ends_with(".mp4") {
                                        "video/mp4"
                                    } else if lower.ends_with(".mp3") {
                                        "audio/mpeg"
                                    } else if lower.ends_with(".ogg") {
                                        "audio/ogg"
                                    } else if lower.ends_with(".opus") {
                                        "audio/opus"
                                    } else if lower.ends_with(".zip") {
                                        "application/zip"
                                    } else {
                                        "application/octet-stream"
                                    };
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
                if matches!(
                    tool_name.as_str(),
                    "write_file" | "write" | "shell" | "execute_command" | "exec"
                ) {
                    let workspace = self
                        .handle
                        .config
                        .workspace
                        .as_deref()
                        .or(self.live.agents.read().await.defaults.workspace.as_deref())
                        .map(expand_tilde)
                        .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

                    // Helper: check if a path is a sendable file type and add to tool_files.
                    let mut try_add_file = |path_str: &str| {
                        let lower = path_str.to_lowercase();
                        let sendable_exts = [
                            ".xlsx", ".xls", ".docx", ".doc", ".pptx", ".ppt", ".pdf", ".csv",
                            ".mp4", ".mp3", ".zip", ".tar.gz", ".txt", ".json", ".html", ".py",
                            ".md",
                        ];
                        if !sendable_exts.iter().any(|ext| lower.ends_with(ext)) {
                            return;
                        }
                        let full = canonicalize_external_path(path_str, &workspace);
                        if !full.exists() {
                            return;
                        }
                        // Skip very large files (>50MB)
                        if let Ok(meta) = full.metadata() {
                            if meta.len() > 50_000_000 {
                                return;
                            }
                        }
                        let filename = full
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        // Avoid duplicates
                        if tool_files
                            .iter()
                            .any(|(_, _, p)| p == &full.to_string_lossy().to_string())
                        {
                            return;
                        }
                        let mime = if lower.ends_with(".xlsx") {
                            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                        } else if lower.ends_with(".docx") {
                            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                        } else if lower.ends_with(".pptx") {
                            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                        } else if lower.ends_with(".pdf") {
                            "application/pdf"
                        } else if lower.ends_with(".csv") {
                            "text/csv"
                        } else if lower.ends_with(".mp4") {
                            "video/mp4"
                        } else if lower.ends_with(".mp3") {
                            "audio/mpeg"
                        } else if lower.ends_with(".zip") {
                            "application/zip"
                        } else {
                            "application/octet-stream"
                        };
                        tool_files.push((
                            filename,
                            mime.to_owned(),
                            full.to_string_lossy().to_string(),
                        ));
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
                                if trimmed.contains('.')
                                    && !trimmed.contains(' ')
                                    && trimmed.len() < 256
                                {
                                    try_add_file(trimmed);
                                }
                            }
                        }
                    }
                }

                // Extract inline images and file attachments from WASM plugin results.
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&result_text) {
                    // data:image/ URIs → tool_images
                    if let Some(imgs) = v.get("images").and_then(|i| i.as_array()) {
                        for img in imgs {
                            if let Some(s) = img.as_str() {
                                if s.starts_with("data:image/") {
                                    tool_images.push(s.to_string());
                                    tracing::info!(
                                        "extracted inline image from tool result ({} bytes)",
                                        s.len()
                                    );
                                }
                            }
                        }
                        if !tool_images.is_empty() {
                            let mut cleaned = v.clone();
                            cleaned["images"] = serde_json::json!(format!(
                                "[{} images extracted as attachments]",
                                tool_images.len()
                            ));
                            result_text = cleaned.to_string();
                        }
                    }

                    // File paths from "files" array → tool_images/tool_files (auto-send)
                    // Jimeng plugin returns: {"files":
                    // ["{\"path\":\"/path/to/1.png\",\"size\":123}", ...]}
                    if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
                        for file_entry in files {
                            let path_str = if let Some(s) = file_entry.as_str() {
                                // May be a JSON string with path field
                                if let Ok(fv) = serde_json::from_str::<serde_json::Value>(s) {
                                    fv.get("path")
                                        .and_then(|p| p.as_str())
                                        .unwrap_or(s)
                                        .to_string()
                                } else {
                                    s.to_string()
                                }
                            } else if let Some(p) = file_entry.get("path").and_then(|p| p.as_str())
                            {
                                p.to_string()
                            } else {
                                continue;
                            };

                            let files_workspace = self
                                .handle
                                .config
                                .workspace
                                .as_deref()
                                .or(self.live.agents.read().await.defaults.workspace.as_deref())
                                .map(expand_tilde)
                                .unwrap_or_else(|| {
                                    crate::config::loader::base_dir().join("workspace")
                                });
                            let pb = canonicalize_external_path(&path_str, &files_workspace);
                            let pb_str = pb.to_string_lossy().to_string();
                            if pb.exists() {
                                let lower = path_str.to_lowercase();
                                let is_image = lower.ends_with(".png")
                                    || lower.ends_with(".jpg")
                                    || lower.ends_with(".jpeg")
                                    || lower.ends_with(".webp");
                                if is_image {
                                    // Push the file path (not base64). The desktop UI loads
                                    // it via Tauri's asset protocol; non-WS channels rehydrate
                                    // to a data URL at the AgentReply boundary
                                    // (`image_ref_to_data_url`), avoiding a multi-MB base64
                                    // blast over the WebSocket.
                                    tool_images.push(pb_str.clone());
                                    tracing::info!(path = %pb_str, "auto-sending image file as attachment (path)");
                                } else {
                                    // Non-image file (video, etc.) → tool_files
                                    let filename = pb
                                        .file_name()
                                        .map(|f| f.to_string_lossy().to_string())
                                        .unwrap_or_else(|| "file".to_string());
                                    let mime = if lower.ends_with(".mp4") {
                                        "video/mp4"
                                    } else if lower.ends_with(".mp3") {
                                        "audio/mpeg"
                                    } else {
                                        "application/octet-stream"
                                    };
                                    tool_files.push((filename, mime.to_string(), pb_str.clone()));
                                    tracing::info!(path = %pb_str, "auto-sending file as attachment");
                                }
                            }
                        }
                        // Clean up result_text
                        if !tool_images.is_empty() || !tool_files.is_empty() {
                            let mut cleaned = v.clone();
                            cleaned["files"] = serde_json::json!(format!(
                                "[{} files auto-sent as attachments]",
                                tool_images.len() + tool_files.len()
                            ));
                            cleaned.as_object_mut().map(|o| o.remove("_action"));
                            result_text = cleaned.to_string();
                        }
                    }
                }

                // Cap or compress tool result for session storage.
                //
                // Routing per tool:
                //   - web_search       -> truncate to limits.web_search (snippets only; no
                //     inline page content since the auto-fetch pipeline was removed).
                //   - web_fetch        -> compress on the flash model when raw exceeds
                //     limits.web_fetch, else truncate.
                //   - web_browser/browser -> compress on the flash model when raw exceeds
                //     limits.web_browser, else truncate. Stateful browser tasks fire many
                //     snapshots per turn, so compression must NOT compete with the primary
                //     model's KV cache — see compress_tool_result_for_session.
                //   - everything else  -> per-tool truncate, with use_skill kept large because
                //     SKILL.md must arrive verbatim.
                let session_text = {
                    use super::web_parsers::truncate_chars;

                    let limits_owned = self
                        .live
                        .ext
                        .read()
                        .await
                        .tools
                        .as_ref()
                        .and_then(|t| t.session_result_limits.clone());
                    let limits = limits_owned.as_ref();

                    let max_chars = match tool_name.as_str() {
                        "shell" | "execute_command" | "exec" => {
                            limits.and_then(|l| l.exec).unwrap_or(3000)
                        }
                        "web_search" => limits.and_then(|l| l.web_search).unwrap_or(2000),
                        "web_fetch" => limits.and_then(|l| l.web_fetch).unwrap_or(5000),
                        "web_browser" | "browser" => {
                            limits.and_then(|l| l.web_browser).unwrap_or(2000)
                        }
                        // use_skill returns SKILL.md, which is a contract
                        // document the LLM MUST see in full. Truncating it
                        // caused the agent to hallucinate CLI invocations
                        // (e.g. flyai's SKILL.md says `npm i -g
                        // @fly-ai/flyai-cli` on line 60 — past the 3000-char
                        // cut — so the agent saw only `runtime: node` in
                        // frontmatter and made up `node index.js` instead).
                        "skill_use" => limits.and_then(|l| l.default).unwrap_or(60_000),
                        "read_file" | "read" => limits.and_then(|l| l.default).unwrap_or(3000),
                        _ => limits.and_then(|l| l.default).unwrap_or(3000),
                    };

                    let needs_compression =
                        matches!(tool_name.as_str(), "web_fetch" | "web_browser" | "browser");

                    if needs_compression && result_text.chars().count() > max_chars {
                        let sk = ctx.session_key.clone();
                        let tn = tool_name.clone();
                        match self
                            .compress_tool_result_for_session(&sk, &tn, &result_text)
                            .await
                        {
                            Ok(summary) => {
                                debug!(
                                    tool = %tn,
                                    orig = result_text.len(),
                                    compressed = summary.len(),
                                    "tool result compressed via flash model"
                                );
                                truncate_chars(&summary, max_chars)
                            }
                            Err(e) => {
                                warn!(tool = %tn, error = %e,
                                    "tool result compression failed, truncating");
                                truncate_chars(&result_text, max_chars)
                            }
                        }
                    } else if result_text.chars().count() > max_chars {
                        truncate_chars(&result_text, max_chars)
                    } else {
                        result_text.clone()
                    }
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
                            // Detect error from result content (exit_code != 0 or error field)
                            is_error: Some(
                                result_text.contains("\"exit_code\":")
                                    && !result_text.contains("\"exit_code\": 0")
                                    || result_text.contains("\"error\"")
                                    || result_text.contains("[stderr]")
                                    || result_text.contains("[exit code:"),
                            ),
                        },
                    ]),
                    rsclaw_hidden: None,
                };
                // Tool results are scratch-paper: keep in the working buffer for
                // this turn's LLM iterations but never persist to session / redb.
                // Only the final assistant text reply enters the conversation history.
                turn_scratchpad.push(tool_msg);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tool dispatch (AGENTS.md §20)
    // -----------------------------------------------------------------------

    fn collect_plugin_tools(&self) -> Vec<PluginToolInfo> {
        let mut out = Vec::new();
        for plugin in self.wasm_plugins.iter() {
            for tool in &plugin.tools {
                out.push(PluginToolInfo {
                    plugin: plugin.name.clone(),
                    runtime: "wasm",
                    tool: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.parameters.clone(),
                });
            }
        }
        if let Some(reg) = self.plugins.as_ref() {
            for (plugin_name, plugin) in reg.js_plugins_iter() {
                for tool in &plugin.manifest.tools {
                    out.push(PluginToolInfo {
                        plugin: plugin_name.clone(),
                        runtime: "js",
                        tool: tool.name.clone(),
                        description: tool.description.clone(),
                        input_schema: tool
                            .input_schema
                            .clone()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    });
                }
            }
        }
        out
    }

    fn find_plugin_tool(&self, plugin: &str, tool: &str) -> Option<PluginToolInfo> {
        self.collect_plugin_tools()
            .into_iter()
            .find(|t| t.plugin == plugin && t.tool == tool)
    }

    fn compact_input_schema(schema: &Value) -> Value {
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut props = serde_json::Map::new();
        if let Some(obj) = schema.get("properties").and_then(|v| v.as_object()) {
            for (name, raw) in obj.iter().take(16) {
                let mut compact = serde_json::Map::new();
                if let Some(t) = raw.get("type") {
                    compact.insert("type".to_owned(), t.clone());
                }
                if let Some(en) = raw.get("enum") {
                    compact.insert("enum".to_owned(), en.clone());
                }
                if let Some(default) = raw.get("default") {
                    compact.insert("default".to_owned(), default.clone());
                }
                if let Some(desc) = raw.get("description").and_then(|d| d.as_str()) {
                    compact.insert(
                        "description".to_owned(),
                        json!(crate::util::truncate_str(desc, 180)),
                    );
                }
                props.insert(name.clone(), Value::Object(compact));
            }
        }
        json!({
            "required": required,
            "properties": props,
        })
    }

    fn plugin_tool_summary(tool: &PluginToolInfo) -> Value {
        json!({
            "plugin": tool.plugin,
            "tool": tool.tool,
            "name": format!("{}.{}", tool.plugin, tool.tool),
            "runtime": tool.runtime,
            "description": crate::util::truncate_str(&tool.description, 280),
            "input_schema_compact": Self::compact_input_schema(&tool.input_schema),
        })
    }

    fn plugin_runtime_priority(runtime: &str) -> u8 {
        match runtime {
            "wasm" => 0,
            "js" => 1,
            _ => 2,
        }
    }

    fn is_cjk_char(ch: char) -> bool {
        matches!(
            ch,
            '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
                | '\u{3040}'..='\u{30FF}'
                | '\u{AC00}'..='\u{D7AF}'
        )
    }

    fn score_plugin_tool(query: &str, tool: &PluginToolInfo) -> i32 {
        let query_l = query.to_lowercase();
        let haystack = format!(
            "{} {} {} {}",
            tool.plugin,
            tool.tool,
            tool.tool.replace('_', " "),
            tool.description
        )
        .to_lowercase();

        let mut score = 0;
        if !query_l.is_empty() && haystack.contains(&query_l) {
            score += 30;
        }
        for token in query_l
            .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '，' | '；' | ':' | '：'))
            .filter(|t| !t.is_empty())
        {
            if tool.tool.to_lowercase().contains(token) {
                score += 12;
            }
            if tool.description.to_lowercase().contains(token) {
                score += 6;
            }
            if tool.plugin.to_lowercase().contains(token) {
                score += 3;
            }
        }

        let mut cjk_total = 0;
        let mut cjk_hits = 0;
        for ch in query_l.chars().filter(|ch| Self::is_cjk_char(*ch)) {
            cjk_total += 1;
            if haystack.contains(ch) {
                cjk_hits += 1;
            }
        }
        if cjk_total > 0 {
            score += cjk_hits * 4;
            if cjk_hits == cjk_total {
                score += 10;
            }
        }
        score
    }

    /// Render an "## Active Plugin Tools" markdown block listing every
    /// `<plugin>.<tool>` selected by `per_plugin`, with the full
    /// `description` and `input_schema`.
    ///
    /// **Superseded by `select_user_tools_pure` as of v1.9** — plugin
    /// tools now flow into `dynamic_prefix.user_tools` as real
    /// structured ToolDefs (their own cache segment), so re-rendering
    /// the same data as `user_system` text would be duplicate work
    /// and waste prompt tokens. Kept for future debug-mode use
    /// (e.g. `/plugin describe <plugin> --text`) and to keep the
    /// resolver primitives importable from tests; not on any live
    /// turn-build path.
    #[allow(dead_code)]
    pub(crate) fn render_active_plugin_tools_text_pure(
        wasm_plugins: &[crate::plugin::wasm_runtime::WasmPlugin],
        js_plugins: Option<&crate::plugin::PluginRegistry>,
        per_plugin: &std::collections::HashMap<String, PluginOverride>,
        cap: usize,
    ) -> Option<String> {
        if per_plugin.is_empty() {
            return None;
        }
        let mut blocks: Vec<String> = Vec::new();
        let mut emitted: usize = 0;
        // WASM first (priority over JS — matches dispatch_tool's
        // wasm-wins ordering for the `<plugin>.<tool>` namespace).
        for wp in wasm_plugins {
            if emitted >= cap {
                break;
            }
            let names = match Self::resolve_plugin_inject_pure(per_plugin, &wp.name) {
                PluginInjectResolution::None => continue,
                PluginInjectResolution::All => wp.tools.iter().map(|t| t.name.clone()).collect(),
                PluginInjectResolution::Names(v) => v,
            };
            let mut lines: Vec<String> = Vec::new();
            for name in &names {
                if emitted >= cap {
                    break;
                }
                if let Some(t) = wp.tools.iter().find(|t| &t.name == name) {
                    lines.push(format_active_plugin_tool_line(
                        &t.name,
                        &t.description,
                        &t.parameters,
                    ));
                    emitted += 1;
                }
            }
            if !lines.is_empty() {
                blocks.push(format!("### {} (wasm)\n{}", wp.name, lines.join("\n")));
            }
        }
        if let Some(reg) = js_plugins {
            for (plugin_name, plugin) in reg.js_plugins_iter() {
                if emitted >= cap {
                    break;
                }
                let names = match Self::resolve_plugin_inject_pure(per_plugin, plugin_name) {
                    PluginInjectResolution::None => continue,
                    PluginInjectResolution::All => plugin
                        .manifest
                        .tools
                        .iter()
                        .map(|t| t.name.clone())
                        .collect(),
                    PluginInjectResolution::Names(v) => v,
                };
                let mut lines: Vec<String> = Vec::new();
                for name in &names {
                    if emitted >= cap {
                        break;
                    }
                    if let Some(t) = plugin.manifest.tools.iter().find(|t| &t.name == name) {
                        let schema = t
                            .input_schema
                            .clone()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                        lines.push(format_active_plugin_tool_line(
                            &t.name,
                            &t.description,
                            &schema,
                        ));
                        emitted += 1;
                    }
                }
                if !lines.is_empty() {
                    blocks.push(format!("### {plugin_name} (js)\n{}", lines.join("\n")));
                }
            }
        }
        if blocks.is_empty() {
            return None;
        }
        Some(format!(
            "## Active Plugin Tools\n\
             These plugin tools are activated for this session. Call each via \
             `plugin_invoke {{plugin, tool, arguments}}` — `arguments` MUST match \
             the `input_schema` below. The host validates required fields before \
             dispatch.\n\n\
             {}",
            blocks.join("\n\n"),
        ))
    }

    /// Render the active-plugins block for this agent's CONFIG-declared
    /// `model.plugin_tools` list (always-on, agent-level).
    ///
    /// **Dead since v1.9**: superseded by `select_user_tools_pure`,
    /// which feeds the same tools into `dynamic_prefix.user_tools` as
    /// real ToolDefs instead of `user_system` text. Kept under
    /// `#[allow(dead_code)]` so a future debug command (`/plugin
    /// inspect`) can call the rendering primitive without us having
    /// to rewrite the wrapper.
    #[allow(dead_code)]
    pub(crate) fn render_config_plugin_tools_text(&self) -> Option<String> {
        const MAX_INJECT_TOOLS: usize = 20;
        let model_cfg = self.handle.config.model.as_ref()?;
        let list = model_cfg.plugin_tools.as_ref()?;
        if list.is_empty() {
            return None;
        }
        let mut map: std::collections::HashMap<String, PluginOverride> =
            std::collections::HashMap::new();
        for entry in list {
            // Accept both `plugin.tool` (preferred) and `plugin/tool` for
            // operators who muscle-memory from skill paths.
            let Some((plugin, tool)) = entry.split_once('.').or_else(|| entry.split_once('/'))
            else {
                tracing::warn!(
                    agent = %self.handle.id,
                    entry = %entry,
                    "plugin_tools entry must be '<plugin>.<tool>'; skipping"
                );
                continue;
            };
            map.entry(plugin.to_owned())
                .or_default()
                .inject
                .push(tool.to_owned());
        }
        Self::render_active_plugin_tools_text_pure(
            &self.wasm_plugins,
            self.plugins.as_deref(),
            &map,
            MAX_INJECT_TOOLS,
        )
    }

    /// Render the active-plugins block for the per-session `/plugin`
    /// slash command overrides. Returns `None` when no override is set.
    ///
    /// **Dead since v1.9**: see `render_active_plugin_tools_text_pure`
    /// docstring for the migration story. The slash-command-driven
    /// overrides now flow through `select_user_tools_pure` and land
    /// in `dynamic_prefix.user_tools`.
    #[allow(dead_code)]
    pub(crate) fn render_session_plugin_tools_text(&self, session_key: &str) -> Option<String> {
        /// Max plugin tools injected per turn (v1 hard cap; v2 swaps for
        /// token-budget). Keeps small-model prompt size under control even
        /// when the user `/plugin xxx all`s a 200-tool plugin.
        const MAX_INJECT_TOOLS: usize = 20;
        let snapshot = match self.handle.plugin_overrides.read() {
            Ok(g) => g.get(session_key).cloned().unwrap_or_default(),
            Err(_) => return None,
        };
        if snapshot.is_empty() {
            return None;
        }
        Self::render_active_plugin_tools_text_pure(
            &self.wasm_plugins,
            self.plugins.as_deref(),
            &snapshot,
            MAX_INJECT_TOOLS,
        )
    }

    /// Pure resolver — what `<plugin>.<tool>` ToolDefs should we inject for
    /// this (session, plugin)? Returns either `Vec::new()` (default / disabled)
    /// or the explicit tool name list. `None` means "expand to all plugin
    /// tools at the call site" (the caller has the plugin metadata).
    ///
    /// Separated from the live-lookup path so unit tests can drive it
    /// without locking the `AgentHandle`'s RwLock.
    pub(crate) fn resolve_plugin_inject_pure(
        per_plugin: &std::collections::HashMap<String, PluginOverride>,
        plugin: &str,
    ) -> PluginInjectResolution {
        match per_plugin.get(plugin) {
            Some(o) if o.disabled => PluginInjectResolution::None,
            Some(o) if o.inject_all => PluginInjectResolution::All,
            Some(o) => PluginInjectResolution::Names(o.inject.clone()),
            None => PluginInjectResolution::None,
        }
    }

    /// Compute the set of plugin tools to expose as real ToolDefs in
    /// `dynamic_prefix.user_tools` for this turn.
    ///
    /// Selection layers (later layers win):
    /// 1. **headline default** — `headline: true` in the plugin's
    ///    `plugin.json5` is the plugin-author-declared baseline. If
    ///    the session override has `inject_all: true` this is replaced
    ///    by the plugin's full tool list; if it has a non-empty
    ///    `inject`, this is replaced by that list.
    /// 2. **per-agent pin** — names in `config_pin` (sourced from
    ///    `model.plugin_tools`) get added on top of the base, even
    ///    when not `headline`-marked.
    /// 3. **session pin** — `PluginOverride.pin` adds for one session
    ///    (slash command `/plugin pin <plugin>__<tool>`).
    /// 4. **per-agent unpin** — `config_unpin` (`model.plugin_tools_unpin`)
    ///    removes names from the resolved base, even headlines.
    /// 5. **session unpin** — `PluginOverride.unpin` removes for one
    ///    session (`/plugin unpin <plugin>__<tool>`).
    ///
    /// Cap is applied last, plugin-by-plugin in declared order
    /// (WASM first, then JS — matches dispatch priority). Excess
    /// tools stay reachable via the `plugin_invoke` meta-tool at
    /// zero prompt-token cost.
    ///
    /// Pure / no I/O — driven entirely by inputs so unit tests can
    /// exercise it without the runtime state.
    pub(crate) fn select_user_tools_pure(
        wasm_plugins: &[crate::plugin::wasm_runtime::WasmPlugin],
        js_plugins: Option<&crate::plugin::PluginRegistry>,
        per_plugin: &std::collections::HashMap<String, PluginOverride>,
        config_pin: &[String],
        config_unpin: &[String],
        cap: usize,
    ) -> Vec<PluginUserToolSelection> {
        // Bucket config-level pin/unpin by plugin so per-plugin lookup
        // is O(active tools) instead of O(config_entries × active_tools).
        let config_pin_by_plugin = bucket_qualified_names(config_pin);
        let config_unpin_by_plugin = bucket_qualified_names(config_unpin);

        let mut selected: Vec<PluginUserToolSelection> = Vec::new();

        // Inner helper closure: collect from one plugin's tool list,
        // honoring all five selection layers.
        let mut take_from_plugin = |plugin_name: &str,
                                    tool_iter: Vec<(String, String, Value, bool)>|
         -> bool {
            // Returns `false` when cap was reached and the caller
            // should stop iterating further plugins.
            let session = per_plugin.get(plugin_name);
            if session.map(|o| o.disabled).unwrap_or(false) {
                return true;
            }

            // Build the candidate set as an ordered name list so the
            // wire output is stable across runs (HashMap iteration is
            // not). Walk the plugin's declared tools in manifest order
            // and decide inclusion.
            let base_names: std::collections::BTreeSet<String> =
                if session.map(|o| o.inject_all).unwrap_or(false) {
                    tool_iter.iter().map(|(n, _, _, _)| n.clone()).collect()
                } else if let Some(o) = session.filter(|o| !o.inject.is_empty()) {
                    o.inject.iter().cloned().collect()
                } else {
                    tool_iter
                        .iter()
                        .filter(|(_, _, _, hl)| *hl)
                        .map(|(n, _, _, _)| n.clone())
                        .collect()
                };

            // pin: add (config + session)
            let mut effective = base_names;
            if let Some(set) = config_pin_by_plugin.get(plugin_name) {
                for name in set {
                    effective.insert(name.clone());
                }
            }
            if let Some(o) = session {
                for name in &o.pin {
                    effective.insert(name.clone());
                }
            }

            // unpin: subtract (config + session). Subtract LAST so
            // unpin always wins a tie with pin.
            if let Some(set) = config_unpin_by_plugin.get(plugin_name) {
                for name in set {
                    effective.remove(name);
                }
            }
            if let Some(o) = session {
                for name in &o.unpin {
                    effective.remove(name);
                }
            }

            // Emit in manifest order so the wire bytes are stable.
            for (name, description, input_schema, _) in tool_iter {
                if !effective.contains(&name) {
                    continue;
                }
                if selected.len() >= cap {
                    return false;
                }
                selected.push(PluginUserToolSelection {
                    plugin_name: plugin_name.to_owned(),
                    tool_name: name,
                    description,
                    input_schema,
                });
            }
            true
        };

        // WASM first (matches dispatch_tool's wasm-wins ordering).
        for wp in wasm_plugins {
            let tools: Vec<(String, String, Value, bool)> = wp
                .tools
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        t.description.clone(),
                        t.parameters.clone(),
                        t.headline,
                    )
                })
                .collect();
            if !take_from_plugin(&wp.name, tools) {
                return selected;
            }
        }
        if let Some(reg) = js_plugins {
            for (plugin_name, plugin) in reg.js_plugins_iter() {
                let tools: Vec<(String, String, Value, bool)> = plugin
                    .manifest
                    .tools
                    .iter()
                    .map(|t| {
                        let schema = t
                            .input_schema
                            .clone()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                        (t.name.clone(), t.description.clone(), schema, t.headline)
                    })
                    .collect();
                if !take_from_plugin(plugin_name, tools) {
                    return selected;
                }
            }
        }
        selected
    }

    /// Set or update a plugin override for a session. Called by the `/plugin`
    /// slash command handler via `&AgentHandle`.
    pub fn set_plugin_override(
        handle: &crate::agent::AgentHandle,
        session_key: &str,
        plugin: &str,
        override_: PluginOverride,
    ) {
        if let Ok(mut g) = handle.plugin_overrides.write() {
            g.entry(session_key.to_owned())
                .or_default()
                .insert(plugin.to_owned(), override_);
        }
    }

    /// Remove all plugin overrides for a session (e.g. `/plugin reset`).
    pub fn clear_plugin_overrides(handle: &crate::agent::AgentHandle, session_key: &str) {
        if let Ok(mut g) = handle.plugin_overrides.write() {
            g.remove(session_key);
        }
    }

    /// In-place mutate the session override for `(session_key, plugin)`,
    /// creating a default entry if none exists. Used by additive
    /// commands (`/plugin pin` and `/plugin unpin`) that modify a single
    /// field without replacing the whole override the way
    /// `set_plugin_override` does. Holds the write lock across the
    /// closure — keep the closure cheap.
    pub fn mutate_plugin_override<F>(
        handle: &crate::agent::AgentHandle,
        session_key: &str,
        plugin: &str,
        mutate: F,
    ) where
        F: FnOnce(&mut PluginOverride),
    {
        if let Ok(mut g) = handle.plugin_overrides.write() {
            let plugin_map = g.entry(session_key.to_owned()).or_default();
            mutate(plugin_map.entry(plugin.to_owned()).or_default());
        }
    }

    pub(crate) async fn tool_plugin_info(&self, args: Value) -> Result<Value> {
        let plugin_filter = args["plugin"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut by_plugin =
            std::collections::BTreeMap::<String, (&'static str, Vec<PluginToolInfo>)>::new();
        for tool in self.collect_plugin_tools() {
            if plugin_filter.is_none_or(|p| tool.plugin == p) {
                by_plugin
                    .entry(tool.plugin.clone())
                    .or_insert((tool.runtime, Vec::new()))
                    .1
                    .push(tool);
            }
        }

        let plugins = by_plugin
            .into_iter()
            .map(|(plugin, (runtime, mut tools))| {
                tools.sort_by(|a, b| a.tool.cmp(&b.tool));
                let common_tools = tools
                    .iter()
                    .take(12)
                    .map(|tool| {
                        json!({
                            "tool": tool.tool,
                            "name": format!("{}.{}", plugin, tool.tool),
                            "description": crate::util::truncate_str(&tool.description, 180),
                        })
                    })
                    .collect::<Vec<_>>();
                json!({
                    "plugin": plugin,
                    "runtime": runtime,
                    "tool_count": tools.len(),
                    "common_tools": common_tools,
                })
            })
            .collect::<Vec<_>>();

        Ok(json!({
            "plugin": plugin_filter,
            "plugins": plugins,
            "next_steps": [
                "Use plugin_search to find task-specific tools.",
                "Use plugin_describe to inspect exact input schema.",
                "Use plugin_invoke to execute a plugin tool."
            ]
        }))
    }

    pub(crate) async fn tool_plugin_search_tools(&self, args: Value) -> Result<Value> {
        Ok(Self::search_plugin_tools_pure(
            self.collect_plugin_tools(),
            &args,
        ))
    }

    /// Pure resolver — same logic as the tool dispatch, but takes the tool
    /// list as input so unit tests can drive it without constructing a full
    /// `AgentRuntime`. Two modes:
    /// - **search**: non-empty `query` → score + rank + paginate.
    /// - **browse**: empty `query` + non-empty `plugin` → list-all
    ///   alphabetical, paginated via `offset`/`limit`. Lets the model walk a
    ///   giant plugin (e.g. douyin's 208 tools) without inventing a new
    ///   meta-tool.
    pub(crate) fn search_plugin_tools_pure(all_tools: Vec<PluginToolInfo>, args: &Value) -> Value {
        let query = args["query"].as_str().unwrap_or("").trim();
        let plugin_filter = args["plugin"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 50) as usize;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;

        if query.is_empty() && plugin_filter.is_none() {
            return json!({
                "error": "plugin_search requires either `query` (search) or `plugin` (browse).",
                "hint": "Examples: {plugin:\"douyin\",query:\"publish video\"} or {plugin:\"douyin\"} to list all tools."
            });
        }

        if query.is_empty() {
            let plugin = plugin_filter.unwrap();
            let mut tools: Vec<PluginToolInfo> = all_tools
                .into_iter()
                .filter(|t| t.plugin == plugin)
                .collect();
            tools.sort_by(|a, b| a.tool.cmp(&b.tool));
            let total = tools.len();
            let page: Vec<Value> = tools
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|t| Self::plugin_tool_summary(&t))
                .collect();
            let next_offset = if offset + page.len() < total {
                json!(offset + page.len())
            } else {
                Value::Null
            };
            return json!({
                "plugin": plugin,
                "mode": "list",
                "total": total,
                "offset": offset,
                "limit": limit,
                "tools": page,
                "next_offset": next_offset,
            });
        }

        let mut scored: Vec<(i32, PluginToolInfo)> = all_tools
            .into_iter()
            .filter(|t| plugin_filter.is_none_or(|p| t.plugin == p))
            .map(|t| (Self::score_plugin_tool(query, &t), t))
            .filter(|(score, _)| *score > 0 || plugin_filter.is_some())
            .collect();

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| {
                    Self::plugin_runtime_priority(a.1.runtime)
                        .cmp(&Self::plugin_runtime_priority(b.1.runtime))
                })
                .then_with(|| a.1.plugin.cmp(&b.1.plugin))
                .then_with(|| a.1.tool.cmp(&b.1.tool))
        });

        let total = scored.len();
        let page: Vec<Value> = scored
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(score, tool)| {
                let mut summary = Self::plugin_tool_summary(&tool);
                summary["score"] = json!(score);
                summary
            })
            .collect();
        let next_offset = if offset + page.len() < total {
            json!(offset + page.len())
        } else {
            Value::Null
        };

        json!({
            "query": query,
            "plugin": plugin_filter,
            "mode": "search",
            "total": total,
            "offset": offset,
            "limit": limit,
            "tools": page,
            "next_offset": next_offset,
        })
    }

    pub(crate) async fn tool_plugin_describe_tool(&self, args: Value) -> Result<Value> {
        let plugin = args["plugin"].as_str().unwrap_or("").trim();
        let tool = args["tool"].as_str().unwrap_or("").trim();
        if plugin.is_empty() || tool.is_empty() {
            return Ok(json!({"error": "plugin_describe requires plugin and tool"}));
        }
        let Some(info) = self.find_plugin_tool(plugin, tool) else {
            return Ok(json!({
                "error": format!("plugin tool not found: {plugin}.{tool}"),
                "hint": "Use plugin_search to discover installed plugin tools."
            }));
        };
        Ok(json!({
            "plugin": info.plugin,
            "tool": info.tool,
            "name": format!("{}.{}", info.plugin, info.tool),
            "runtime": info.runtime,
            "description": info.description,
            "input_schema": info.input_schema,
        }))
    }

    fn validate_plugin_arguments(
        tool: &PluginToolInfo,
        args: &Value,
    ) -> std::result::Result<(), Value> {
        if !args.is_object() {
            return Err(json!({
                "error": "plugin_invoke arguments must be an object",
                "plugin": tool.plugin,
                "tool": tool.tool,
                "schema_hint": Self::compact_input_schema(&tool.input_schema),
            }));
        }
        if let Some(required) = tool.input_schema.get("required").and_then(|v| v.as_array()) {
            let missing: Vec<&str> = required
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|key| args.get(*key).is_none_or(Value::is_null))
                .collect();
            if !missing.is_empty() {
                return Err(json!({
                    "error": "plugin_invoke missing required arguments",
                    "plugin": tool.plugin,
                    "tool": tool.tool,
                    "missing": missing,
                    "schema_hint": Self::compact_input_schema(&tool.input_schema),
                }));
            }
        }
        Ok(())
    }

    pub(crate) async fn tool_plugin_invoke(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let plugin_name = args["plugin"].as_str().unwrap_or("").trim();
        let tool_name = args["tool"].as_str().unwrap_or("").trim();
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if plugin_name.is_empty() || tool_name.is_empty() {
            return Ok(json!({"error": "plugin_invoke requires plugin, tool, and arguments"}));
        }
        let Some(info) = self.find_plugin_tool(plugin_name, tool_name) else {
            return Ok(json!({
                "error": format!("plugin tool not found: {plugin_name}.{tool_name}"),
                "hint": "Use plugin_search to discover installed plugin tools."
            }));
        };
        if let Err(err) = Self::validate_plugin_arguments(&info, &arguments) {
            return Ok(err);
        }

        if let Some(wp) = self.wasm_plugins.iter().find(|p| p.name == plugin_name) {
            let notify_ctx = self.notification_tx.as_ref().map(|tx| {
                crate::plugin::wasm_runtime::WasmNotifyCtx {
                    tx: tx.clone(),
                    target_id: if !ctx.chat_id.is_empty() {
                        ctx.chat_id.clone()
                    } else {
                        ctx.peer_id.clone()
                    },
                    channel: ctx.channel.clone(),
                }
            });
            return wp
                .call_tool_with_ctx(tool_name, arguments, notify_ctx)
                .await;
        }

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
                "args": arguments,
                "_ctx": {
                    "target_id": target_id,
                    "channel": ctx.channel.clone(),
                    "session_key": ctx.session_key.clone(),
                }
            });
            return plugin.call("tool_call", params).await;
        }

        Ok(json!({"error": format!("plugin runtime not loaded: {plugin_name}")}))
    }

    /// The exact whitelist `dispatch_tool` should enforce, matching what
    /// `build_tool_list` exposes to the LLM. Returns None when the agent's
    /// config doesn't constrain its toolset (the default-everything case),
    /// in which case the dispatcher accepts any name the match arms below
    /// recognize. Returns Some(set) only when the agent explicitly sets
    /// `model.toolset` or `model.tools[]`, so deliberately-scoped agents
    /// like the hub router can refuse hallucinated tool names.
    fn allowed_tools_for_dispatch(&self) -> Option<std::collections::HashSet<String>> {
        let model_cfg = self.handle.config.model.as_ref()?;
        let explicit_toolset = model_cfg.toolset.as_deref();
        let custom_tools = model_cfg.tools.as_ref();
        if explicit_toolset.is_none() && custom_tools.is_none() {
            return None;
        }
        crate::agent::tools_builder::toolset_allowed_names(
            explicit_toolset.unwrap_or("standard"),
            custom_tools,
        )
    }

    async fn dispatch_tool(
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

        // 2. Built-in tools (checked before A2A prefix so reserved names are not
        //    hijacked).
        match name {
            // --- Consolidated tools (new unified names) ---
            "memory" => return self.tool_memory_consolidated(ctx, args).await,
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
                    .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));
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
                            .find(|m| m.role == crate::provider::Role::User)
                            .and_then(|m| match &m.content {
                                crate::provider::MessageContent::Text(t) => Some(t.as_str()),
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
            "image_gen" | "image" => return self.tool_image(args).await,
            "video_gen" | "video" => return self.tool_video(args, ctx).await,
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
            "codex" => return self.tool_codex(ctx, args).await,
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
        //    OpenAI-name-compatible). Legacy `<plugin>.<tool>` from older
        //    transcripts is still accepted by trying the new separator
        //    first and falling back. Wasm wins on collision; must precede
        //    skill match because plugins are higher in the priority ladder.
        if let Some((plugin_name, tool_name)) = name
            .split_once(PLUGIN_TOOL_SEP)
            .or_else(|| name.split_once('.'))
        {
            if let Some(wp) = self.wasm_plugins.iter().find(|p| p.name == plugin_name) {
                let notify_ctx = self.notification_tx.as_ref().map(|tx| {
                    crate::plugin::wasm_runtime::WasmNotifyCtx {
                        tx: tx.clone(),
                        target_id: if !ctx.chat_id.is_empty() {
                            ctx.chat_id.clone()
                        } else {
                            ctx.peer_id.clone()
                        },
                        channel: ctx.channel.clone(),
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

        // 2. Fall back to remote A2A gateway (Level 3).
        // Normalize: LLMs sometimes replace _ with - in tool names.
        let normalized_id = agent_id.replace('-', "_");
        if let Some(ext) = self
            .config
            .agents
            .a2a
            .iter()
            .find(|e| e.id == agent_id || e.id == normalized_id)
        {
            use crate::a2a::client::A2aClient;
            let client = A2aClient::new();
            // Use remote agent ID if configured, otherwise omit (uses remote default).
            let remote_id = ext.remote_agent_id.as_deref().unwrap_or("");
            // Resolve the peer token (plain / ${ENV} / secret-ref) at call time.
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

    /// `knowledge_base` tool — read (hybrid search) plus user-directed write
    /// (build a collection / ingest text the agent prepared). Reads the
    /// process-global KnowledgeService (opened at gateway startup; redb is
    /// exclusively locked, so we never re-open it here). Write actions run in
    /// spawn_blocking (sync redb writes).
    pub(crate) async fn tool_knowledge_base(&self, args: Value) -> Result<Value> {
        let action = args["action"].as_str().unwrap_or("search").trim();
        let Some(kb) = crate::kb::global_service() else {
            return Ok(json!({"results": [], "note": "knowledge base not available"}));
        };
        match action {
            "search" | "" => self.kb_action_search(kb, args).await,
            "list_collections" => {
                let cols = tokio::task::spawn_blocking(move || kb.list_collections())
                    .await
                    .map_err(|e| anyhow::anyhow!("kb list task failed: {e}"))?
                    .map_err(|e| anyhow::anyhow!("kb list_collections failed: {e}"))?;
                let list: Vec<Value> = cols
                    .into_iter()
                    .map(|c| json!({"id": c.id, "name": c.name, "description": c.description}))
                    .collect();
                Ok(json!({ "collections": list }))
            }
            "create_collection" => {
                let name = args["name"].as_str().unwrap_or("").trim().to_owned();
                if name.is_empty() {
                    return Ok(json!({"error": "name required for create_collection"}));
                }
                let desc = args["description"]
                    .as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                let (id, name, created) = tokio::task::spawn_blocking(move || {
                    resolve_or_create_collection(&kb, &name, desc)
                })
                .await
                .map_err(|e| anyhow::anyhow!("kb create task failed: {e}"))??;
                Ok(json!({ "collection_id": id, "name": name, "created": created }))
            }
            "add" => {
                let coll = args["collection"].as_str().unwrap_or("").trim().to_owned();
                let title = args["title"].as_str().unwrap_or("").trim().to_owned();
                let content = args["content"].as_str().unwrap_or("").to_owned();
                if coll.is_empty() || title.is_empty() || content.trim().is_empty() {
                    return Ok(json!({"error": "add requires collection, title, and content"}));
                }
                // Agent-authored content is markdown/plain text; default to
                // markdown (the md canonicalizer handles plain text fine).
                let mime = args["mime"]
                    .as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "text/markdown".to_owned());
                let out = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
                    let (cid, _name, _created) = resolve_or_create_collection(&kb, &coll, None)?;
                    let (doc_id, noop) = kb
                        .ingest(&cid, &title, content.as_bytes(), Some(&mime))
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    Ok(json!({
                        "doc_id": doc_id,
                        "collection_id": cid,
                        "status": if noop { "duplicate" } else { "pending" },
                    }))
                })
                .await
                .map_err(|e| anyhow::anyhow!("kb add task failed: {e}"))??;
                Ok(out)
            }
            other => Ok(json!({
                "error": format!("unknown action '{other}' (use search, add, create_collection, list_collections)")
            })),
        }
    }

    /// The read path of `knowledge_base` (action=search).
    async fn kb_action_search(
        &self,
        kb: std::sync::Arc<crate::kb::KnowledgeService>,
        args: Value,
    ) -> Result<Value> {
        // Trim query defensively (v1 tool-call protocol can leak a trailing
        // newline into string args).
        let query = args["query"].as_str().unwrap_or("").trim().to_owned();
        if query.is_empty() {
            return Ok(json!({"results": [], "note": "empty query"}));
        }
        let collection_ids: Vec<String> = args["collection_ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let top_k = args["top_k"].as_u64().unwrap_or(5).clamp(1, 50) as usize;

        // KnowledgeService::search is synchronous and CPU-heavy (embed + HNSW +
        // tantivy); run it off the async executor so it doesn't stall the turn.
        let hits =
            tokio::task::spawn_blocking(move || kb.search(&query, &collection_ids, top_k, 0.0))
                .await
                .map_err(|e| anyhow::anyhow!("knowledge_base search task failed: {e}"))?
                .map_err(|e| anyhow::anyhow!("knowledge_base search failed: {e}"))?;

        let results: Vec<Value> = hits
            .into_iter()
            .map(|h| {
                json!({
                    "doc_id": h.doc_id,
                    "collection": h.collection_name,
                    "source_title": h.source_title,
                    "text": h.chunk_text,
                    "score": h.score,
                })
            })
            .collect();
        Ok(json!({
            "count": results.len(),
            "results": results,
            "note": if results.is_empty() { "no matching content in the knowledge base — do NOT fabricate a citation" } else { "" },
        }))
    }

    pub(crate) async fn tool_memory_search(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let query = args["query"].as_str().unwrap_or("").trim().to_owned();
        if query.is_empty() {
            return Ok(json!({"results": [], "note": "empty query"}));
        }
        let default_scope = default_memory_scope(&ctx.agent_id, &ctx.channel);
        let scope = args["scope"]
            .as_str()
            .map(|s| normalize_memory_scope(s, &ctx.agent_id))
            .unwrap_or(default_scope);
        let top_k = args["top_k"].as_u64().unwrap_or(5).clamp(1, 25) as usize;

        let Some(ref mem) = self.memory else {
            return Ok(json!({"results": [], "note": "memory store not available"}));
        };
        let docs = self.search_memory_docs(mem, &query, &scope, top_k).await?;
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

    async fn search_memory_docs(
        &self,
        mem: &Arc<Mutex<MemoryStore>>,
        query: &str,
        scope: &str,
        top_k: usize,
    ) -> Result<Vec<MemoryDoc>> {
        let bm25_hits = match self.store.search.search(
            query,
            Some(scope),
            top_k.saturating_mul(4).max(16),
        ) {
            Ok(hits) => hits,
            Err(e) => {
                tracing::debug!(query = %query, scope = %scope, "BM25 memory search skipped: {e:#}");
                Vec::new()
            }
        };
        let mut store = mem.lock().await;
        let vec_docs = store
            .search(query, Some(scope), top_k.saturating_mul(3).max(top_k))
            .await?;
        let bm25_docs: Vec<MemoryDoc> = bm25_hits
            .into_iter()
            .filter_map(|hit| store.get_sync(&hit.id).cloned())
            .collect();
        Ok(rrf_fuse(vec_docs, bm25_docs, top_k))
    }

    async fn build_auto_recall_bundle(
        &self,
        agent_id: &str,
        channel: &str,
        query: &str,
    ) -> Option<RecallBundle> {
        if query.trim().is_empty() || matches!(channel, "heartbeat" | "cron" | "system") {
            return None;
        }
        let enabled = self
            .config
            .agents
            .defaults
            .memory
            .as_ref()
            .and_then(|m| m.auto_recall)
            .unwrap_or(true);
        if !enabled {
            return None;
        }
        let Some(ref mem) = self.memory else {
            return None;
        };
        let memory_cfg = self.config.agents.defaults.memory.as_ref();
        let final_k = memory_cfg
            .and_then(|m| m.recall_final_k)
            .unwrap_or(5)
            .clamp(1, 12);
        let max_tokens = memory_cfg
            .and_then(|m| {
                m.retrieval
                    .as_ref()
                    .and_then(|v| v.get("maxTokens"))
                    .and_then(Value::as_u64)
                    .map(|v| v as usize)
            })
            .unwrap_or(1200)
            .clamp(128, 4096);
        let scope = default_memory_scope(agent_id, channel);
        let docs = match self.search_memory_docs(mem, query, &scope, final_k).await {
            Ok(docs) => docs,
            Err(e) => {
                tracing::debug!(error = %e, "auto recall search failed");
                return None;
            }
        };
        let trace_id = format!("recall_{}", Uuid::new_v4());
        recall_bundle_from_docs(docs, max_tokens, &trace_id)
    }

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

    /// `use_skill` — first-class function-call tool that activates an
    /// installed skill. The whole reason this exists is that the LLM
    /// strongly prefers tools registered in the function-call API over
    /// suggestions buried in the system prompt; without `use_skill` the
    /// model would reach for `web_fetch`/`web_browser` even when a skill's
    /// description matched the task. Returns the skill's full SKILL.md so
    /// the LLM can derive the exact CLI invocation, then call
    /// `shell` to run it.
    /// Escalate the user's current request into a multi-turn background
    /// task. The LLM is the one judging when this is warranted (see the
    /// `task` ToolDef description). The original `looks_like_task`
    /// keyword heuristic regularly mis-classified short Chinese
    /// questions like "你可以帮我做啥？", so the decision moved here.
    pub(crate) async fn tool_task(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        use crate::gateway::task_queue::{
            self, Priority, QueuedMessage, TASK_DEFAULT_MAX_TURNS, TASK_DEFAULT_TTL_SECS,
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

        let Some(manager) = task_queue::get_task_queue() else {
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

        let (task_id, merged) = manager
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

    pub(crate) fn tool_use_skill(&self, args: Value) -> Result<Value> {
        // Try "name" first, then fall back to common alternatives the model
        // sometimes emits when it confuses the parameter name.
        let name = args
            .get("name")
            .or_else(|| args.get("skill"))
            .or_else(|| args.get("skill_name"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let available: Vec<String> = self.skills.all().map(|s| s.name.clone()).collect();
                anyhow!(
                    "use_skill: 'name' is required. Available skills: {}. \
                     Received args: {}",
                    available.join(", "),
                    args
                )
            })?;
        // Resolve the skill dir: prefer the live registry, else fall back to
        // the skills dir on disk so a freshly `skill_install`ed skill is usable
        // the SAME turn (before the next compact/clear/new reload folds it into
        // the registry + system prompt).
        let skill_dir = if let Some(skill) = self.skills.get(name) {
            skill.dir.clone()
        } else if !crate::skill::valid_slug(name) {
            // Disk fallback joins `name` onto the skills root — reject traversal.
            return Ok(serde_json::json!({ "error": format!("invalid skill name: {name:?}") }));
        } else {
            let disk = crate::config::loader::base_dir().join("skills").join(name);
            if disk.join("SKILL.md").is_file() {
                disk
            } else {
                let available: Vec<&str> = self.skills.all().map(|s| s.name.as_str()).collect();
                return Ok(serde_json::json!({
                    "error": format!("skill '{name}' not installed"),
                    "available": available,
                }));
            }
        };
        let dir = skill_dir.display().to_string();
        let skill_md_path = skill_dir.join("SKILL.md");
        let skill_md = std::fs::read_to_string(&skill_md_path).unwrap_or_else(|e| {
            format!(
                "(failed to read SKILL.md: {e}; check {})",
                skill_md_path.display()
            )
        });
        Ok(serde_json::json!({
            "name": name,
            "dir": dir,
            "skill_md": skill_md,
            "next_step": "Read skill_md to find the exact CLI command and flags, \
                          then call shell to run it. \
                          Pass the user's actual question / parameters via the \
                          flags documented in skill_md."
        }))
    }

    /// `skill_list` — list locally-installed skills (name + description).
    pub(crate) fn tool_skill_list(&self, args: Value) -> Result<Value> {
        Ok(paginate_skill_list(self.skills.all(), &args))
    }

    /// `skill_search` — search the remote skill registries (clawhub / skillhub
    /// / iwencai). Returns candidate slugs the model can `skill_install`.
    pub(crate) async fn tool_skill_search(&self, args: Value) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .or_else(|| args["q"].as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if query.is_empty() {
            return Ok(json!({ "error": "query is required" }));
        }
        let client = crate::skill::clawhub::ClawhubClient::new();
        let results = match client.search_with_fallback(&query).await {
            Ok(r) => r,
            Err(e) => return Ok(json!({ "error": format!("skill search failed: {e}") })),
        };
        let out: Vec<Value> = results
            .into_iter()
            .take(20)
            .map(|r| {
                json!({
                    "slug": r.slug,
                    "description": r.description,
                    "installs": r.installs,
                    "registry": r.registry,
                })
            })
            .collect();
        Ok(json!({
            "count": out.len(),
            "results": out,
            "hint": "To use one: skill_install(name=\"<slug>\") then skill_use(name=\"<slug>\").",
        }))
    }

    /// `skill_install` — install a skill from a registry into the global skills
    /// dir. Usable the same turn via `skill_use` (disk fallback); it folds into
    /// the system-prompt skill list on the next compaction/clear/new reload.
    pub(crate) async fn tool_skill_install(&self, args: Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .or_else(|| args["slug"].as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if name.is_empty() {
            return Ok(json!({ "error": "name is required" }));
        }
        if !crate::skill::valid_slug(&name) {
            return Ok(json!({ "error": format!("invalid skill name: {name:?}") }));
        }
        // Security gate: the agent may AUTO-install only audited, content-pinned
        // skills (allowlist). Off-list installs are blocked here and must go
        // through the human-initiated CLI (`rsclaw skills install`).
        let Some(entry) = crate::skill::allowlist::snapshot().lookup_skill(&name) else {
            return Ok(json!({
                "error": format!("'{name}' is not on the audited auto-install allowlist"),
                "guidance": "This skill is not pre-audited, so it cannot be auto-installed. \
                    Tell the user; if they trust it they can install it themselves with \
                    `rsclaw skills install <slug>`.",
            }));
        };
        if entry.url.is_empty() {
            return Ok(json!({ "error": format!("allowlist entry '{name}' has no download url") }));
        }
        let dir = crate::config::loader::base_dir().join("skills");
        let client = crate::skill::clawhub::ClawhubClient::new();
        // Install ONLY from the audited allowlist URL (a direct https:// spec
        // routes through install_from_url, NOT the public registry search).
        match client.install_with_fallback(&entry.url, &dir).await {
            Ok(locked) => {
                // Content pin: the downloaded SKILL.md must match the audited
                // hash, so a registry can't swap content under an audited slug.
                if let Err(e) =
                    crate::skill::allowlist::verify_skill_content(&locked.install_dir, &entry, true)
                {
                    let _ = std::fs::remove_dir_all(&locked.install_dir);
                    return Ok(json!({ "error": e.to_string(), "name": name }));
                }
                Ok(json!({
                    "installed": name,
                    "version": locked.version,
                    "dir": locked.install_dir.display().to_string(),
                    "next_step": "Usable now: skill_use(name=\"...\") to read its SKILL.md, then run the CLI it documents via shell.",
                }))
            }
            Err(e) => Ok(json!({ "error": format!("install failed: {e}"), "name": name })),
        }
    }

    /// `skill_remove` — uninstall a locally-installed skill (delete its dir).
    pub(crate) async fn tool_skill_remove(&self, args: Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .or_else(|| args["slug"].as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if name.is_empty() {
            return Ok(json!({ "error": "name is required" }));
        }
        // Path-traversal guard: `name` is model-controlled and joined onto the
        // skills root, then `remove_dir_all`'d. Without this, name = "../../.ssh"
        // would delete a directory outside skills. Reject anything non-slug.
        if !crate::skill::valid_slug(&name) {
            return Ok(json!({ "error": format!("invalid skill name: {name:?}") }));
        }
        let dir = crate::config::loader::base_dir().join("skills").join(&name);
        if !dir.is_dir() {
            return Ok(json!({ "error": format!("skill '{name}' not installed") }));
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(_) => Ok(json!({
                "removed": name,
                "note": "Removed from disk. Drops from the system-prompt skill list on the next compaction/clear/new reload.",
            })),
            Err(e) => Ok(json!({ "error": format!("remove failed: {e}") })),
        }
    }

    pub(crate) async fn tool_memory_put(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let text = args["text"].as_str().unwrap_or("").trim().to_owned();
        if text.is_empty() {
            return Ok(json!({"stored": false, "note": "empty memory text"}));
        }
        // Internal channels (heartbeat/cron/system) get a separate scope so
        // their memories don't pollute normal conversation auto-recall.
        let default_scope = default_memory_scope(&ctx.agent_id, &ctx.channel);
        let scope = args["scope"]
            .as_str()
            .map(|s| normalize_memory_scope(s, &ctx.agent_id))
            .unwrap_or(default_scope);
        let kind = normalize_memory_kind(args["kind"].as_str());
        let id = args["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut importance = default_memory_importance(&kind);
        if let Some(v) = args["importance"].as_f64() {
            importance = (v as f32).clamp(0.01, 1.0);
        }
        let tier = default_memory_doc_tier(&kind);
        let pinned = kind == "entity" || args["pinned"].as_bool().unwrap_or(false);
        let tags = if pinned {
            vec!["pinned".to_owned()]
        } else {
            Vec::new()
        };

        let Some(ref mem) = self.memory else {
            return Ok(json!({"error": "memory store not available"}));
        };
        // Off-lock add: BERT inference happens between two brief lock
        // windows so concurrent reads/writes don't stall on it.
        crate::agent::memory::add_off_lock(
            mem,
            MemoryDoc {
                id: id.clone(),
                scope: scope.clone(),
                kind: kind.clone(),
                text: text.clone(),
                vector: vec![],
                created_at: 0,
                accessed_at: 0,
                access_count: 0,
                importance,
                tier,
                abstract_text: None,
                overview_text: None,
                tags,
                pinned,
            },
        )
        .await?;
        let (effective_id, effective_scope, effective_kind, effective_text) = {
            let store = mem.lock().await;
            store
                .find_exact(&scope, &kind, &text)
                .map(|doc| {
                    (
                        doc.id.clone(),
                        doc.scope.clone(),
                        doc.kind.clone(),
                        doc.text.clone(),
                    )
                })
                .unwrap_or_else(|| (id.clone(), scope.clone(), kind.clone(), text.clone()))
        };
        // Also index in tantivy BM25 for hybrid search.
        if let Err(e) = self.store.search.index_memory_doc(
            &effective_id,
            &effective_scope,
            &effective_kind,
            &effective_text,
        ) {
            tracing::warn!("BM25 index failed for memory_put doc: {e:#}");
        }
        // Only append to MEMORY.md for user-initiated /remember commands,
        // not for automatic memory_put calls by the model.
        if kind != "remember" {
            return Ok(
                json!({"stored": true, "id": effective_id, "scope": effective_scope, "kind": effective_kind}),
            );
        }
        // Snapshot defaults.workspace before the chain — `.or_else` takes a
        // sync closure so we can't await inside it.
        let default_workspace = self.live.agents.read().await.defaults.workspace.clone();
        let ws_str = self
            .handle
            .config
            .workspace
            .clone()
            .or(default_workspace)
            .unwrap_or_else(|| {
                crate::config::loader::base_dir()
                    .join("workspace")
                    .to_string_lossy()
                    .into_owned()
            });
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
        Ok(
            json!({"stored": true, "id": effective_id, "scope": effective_scope, "kind": effective_kind}),
        )
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
        crate::provider::model_defaults::resolve_max_tokens(provider_name, model_id, None);
    (resolved > 0).then_some(resolved)
}

/// Canonicalize a path received from an external source (tool output, plugin
/// result, LLM-generated argument). Performs:
///   1. `~/...` expansion via [`expand_tilde`]
///   2. If still relative, joins with `workspace`
///   3. Collapses `.` and `..` components without requiring filesystem access
///      (does NOT follow symlinks — this is a pure lexical normalization).
///
/// This is the single entry point for turning untrusted path strings into
/// filesystem paths the host will actually read/write. Call this instead of
/// `PathBuf::from(s)` at module boundaries.
pub(crate) fn canonicalize_external_path(
    input: &str,
    workspace: &std::path::Path,
) -> std::path::PathBuf {
    use std::path::Component;
    let expanded = expand_tilde(input);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        workspace.join(expanded)
    };
    let mut out = std::path::PathBuf::new();
    for c in absolute.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
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
        #[allow(unused_mut)]
        let mut pdf_cmd = std::process::Command::new("pdftotext");
        pdf_cmd.args([tmp.to_str().unwrap_or(""), "-"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            pdf_cmd.creation_flags(0x08000000);
        }
        let output = pdf_cmd.output();
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
                .or_else(|| r.get("summary"))
                .or_else(|| r.get("content"))
                .or_else(|| r.get("slug"))
                .or_else(|| r.get("name"))
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
    bm25_hits: Vec<crate::agent::memory::MemoryDoc>,
    top_k: usize,
) -> Vec<crate::agent::memory::MemoryDoc> {
    use std::collections::HashMap;

    const K: f32 = 60.0;

    // score_map: doc_id → (rrf_score, MemoryDoc)
    let mut scores: HashMap<String, (f32, crate::agent::memory::MemoryDoc)> = HashMap::new();

    // Vector hits — rank 1-based.
    for (rank, doc) in vec_hits.into_iter().enumerate() {
        let rrf = 1.0 / (K + (rank + 1) as f32);
        scores
            .entry(doc.id.clone())
            .and_modify(|(s, _)| *s += rrf)
            .or_insert((rrf, doc));
    }

    // BM25 hits — rank 1-based.
    for (rank, doc) in bm25_hits.into_iter().enumerate() {
        let rrf = 1.0 / (K + (rank + 1) as f32);
        scores
            .entry(doc.id.clone())
            .and_modify(|(s, _)| *s += rrf)
            .or_insert((rrf, doc));
    }

    // Apply lifecycle + quality multiplier. Pinned/Core/important docs should
    // beat low-value notes when both are plausible matches.
    let mut ranked: Vec<(f32, MemoryDoc)> = scores
        .into_values()
        .map(|(score, doc)| {
            let quality = if doc.pinned { 1.25 } else { 1.0 }
                * match doc.tier {
                    MemDocTier::Core => 1.15,
                    MemDocTier::Working => 1.0,
                    MemDocTier::Peripheral => 0.8,
                }
                * (0.75 + doc.importance.clamp(0.01, 1.0) * 0.5);
            (score * doc.decay_multiplier() * quality, doc)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().take(top_k).map(|(_, doc)| doc).collect()
}

// ---------------------------------------------------------------------------
// Tool dispatch helpers — inject fields for backward-compat routing
// ---------------------------------------------------------------------------

fn default_memory_scope(agent_id: &str, channel: &str) -> String {
    if matches!(channel, "heartbeat" | "cron" | "system") {
        format!("agent:{agent_id}:{channel}")
    } else {
        format!("agent:{agent_id}")
    }
}

/// Resolve a KB collection by name, creating it if absent. Returns
/// `(id, name, created)`. Used by the `knowledge_base` write actions so the
/// agent can target a collection by human name ("会议记录") without first
/// looking up its id. Name match is case-insensitive; a create racing with a
/// concurrent create falls back to the existing one.
fn resolve_or_create_collection(
    kb: &crate::kb::KnowledgeService,
    name: &str,
    desc: Option<String>,
) -> anyhow::Result<(String, String, bool)> {
    let find = || -> anyhow::Result<Option<crate::kb::model::KbCollection>> {
        Ok(kb
            .list_collections()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .into_iter()
            .find(|c| c.name.eq_ignore_ascii_case(name)))
    };
    if let Some(c) = find()? {
        return Ok((c.id, c.name, false));
    }
    match kb.create_collection(name, desc, None) {
        Ok(c) => Ok((c.id, c.name, true)),
        Err(crate::kb::KnowledgeError::DuplicateName) => {
            let c =
                find()?.ok_or_else(|| anyhow::anyhow!("collection vanished after duplicate"))?;
            Ok((c.id, c.name, false))
        }
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

fn normalize_memory_scope(scope: &str, agent_id: &str) -> String {
    let scope = scope.trim();
    if scope.is_empty() {
        return format!("agent:{agent_id}");
    }
    if scope == agent_id {
        return format!("agent:{agent_id}");
    }
    scope.to_owned()
}

fn normalize_memory_kind(kind: Option<&str>) -> String {
    match kind.unwrap_or("fact").trim() {
        "remember" => "remember",
        "entity" => "entity",
        "preference" => "preference",
        "procedure" => "procedure",
        "summary" => "summary",
        "note" => "note",
        "fact" | "" => "fact",
        _ => "fact",
    }
    .to_owned()
}

fn default_memory_importance(kind: &str) -> f32 {
    match kind {
        "entity" => 0.95,
        "remember" => 0.8,
        "preference" => 0.75,
        "procedure" => 0.65,
        "summary" => 0.55,
        "note" => 0.3,
        _ => 0.65,
    }
}

fn default_memory_doc_tier(kind: &str) -> MemDocTier {
    match kind {
        "entity" => MemDocTier::Core,
        "note" => MemDocTier::Peripheral,
        _ => MemDocTier::Working,
    }
}

fn recall_bundle_from_docs(
    docs: Vec<MemoryDoc>,
    max_tokens: usize,
    trace_id: &str,
) -> Option<RecallBundle> {
    let mut lines = Vec::new();
    let mut doc_ids = Vec::new();
    let mut used_tokens = 0usize;
    let mut truncated = false;

    for doc in docs {
        if doc.kind == "note" {
            continue;
        }
        let text = doc.display_text().trim();
        if text.is_empty() {
            continue;
        }
        let line = format!("- {text}");
        let line_tokens = estimate_tokens(&line);
        if used_tokens + line_tokens > max_tokens {
            truncated = true;
            if lines.is_empty() {
                let char_limit = max_tokens.saturating_mul(4).max(64);
                let clipped: String = line.chars().take(char_limit).collect();
                lines.push(clipped);
                doc_ids.push(doc.id);
            }
            break;
        }
        used_tokens += line_tokens;
        lines.push(line);
        doc_ids.push(doc.id);
    }

    if lines.is_empty() {
        return None;
    }
    let context = lines.join("\n");
    let hash = {
        use sha2::{Digest, Sha256};
        format!("sha256:{:x}", Sha256::digest(context.as_bytes()))
    };
    Some(RecallBundle {
        context,
        metadata: RecallMetadata {
            mode: "committed".to_owned(),
            format: "xml".to_owned(),
            source: "server".to_owned(),
            trace_id: Some(trace_id.to_owned()),
            max_tokens: Some(max_tokens as u32),
            doc_ids,
            hash,
            truncated,
        },
    })
}

/// Inject an `action` field into `args` if not already present.
fn inject_action(mut args: Value, action: &str) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.entry("action").or_insert_with(|| json!(action));
    }
    args
}

/// Backward-compatible `/remember`/`memory_put` routing should preserve the
/// explicit user intent instead of falling through to the generic fact default.
fn inject_memory_put_compat(mut args: Value) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.entry("action").or_insert_with(|| json!("put"));
        obj.entry("kind").or_insert_with(|| json!("remember"));
    }
    args
}

/// Force `auth=true` on the wait-input args so the `wait_auth` alias
/// routes to the AUTH_REQUIRED variant of the suspend-resume bridge.
fn inject_auth(mut args: Value) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.insert("auth".to_owned(), json!(true));
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

    let config_path = crate::config::loader::detect_config_path()
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
    entry: &crate::config::schema::AgentEntry,
) -> anyhow::Result<()> {
    let config_path = crate::config::loader::detect_config_path()
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
mod tests {
    use super::*;
    use crate::{
        agent::context_mgr::msg_chars,
        config::schema::{ContextPruningConfig, HardClearConfig, SoftTrimConfig},
        provider::{Message, MessageContent, Role},
        skill::SkillRegistry,
    };

    // ---------------------------------------------------------------
    // resolve_default_workspace — locks in the rule that file tools
    // (list_dir, search_file, search_content, read, write, edit, shell)
    // default to the agent's workspace, NEVER to "." / CWD / $HOME.
    // ---------------------------------------------------------------

    #[test]
    fn workspace_per_agent_override_beats_default_and_fallback() {
        let base = std::path::Path::new("/tmp/some-base");
        let got = resolve_default_workspace(Some("/agents/me"), Some("/agents/all"), base);
        assert_eq!(got, std::path::PathBuf::from("/agents/me"));
    }

    #[test]
    fn workspace_global_default_used_when_no_per_agent_override() {
        let base = std::path::Path::new("/tmp/some-base");
        let got = resolve_default_workspace(None, Some("/agents/all"), base);
        assert_eq!(got, std::path::PathBuf::from("/agents/all"));
    }

    #[test]
    fn workspace_falls_back_to_base_dir_join_workspace() {
        let base = std::path::Path::new("/tmp/some-base");
        let got = resolve_default_workspace(None, None, base);
        // The `main` agent in production hits this branch — it has no
        // per-agent override and the defaults.workspace isn't set, so
        // every file tool resolves to <base_dir>/workspace.
        assert_eq!(got, std::path::PathBuf::from("/tmp/some-base/workspace"));
    }

    #[test]
    fn workspace_tilde_is_expanded() {
        let base = std::path::Path::new("/tmp/some-base");
        let got = resolve_default_workspace(Some("~/myws"), None, base);
        let home = dirs_next::home_dir().expect("home dir for test");
        assert_eq!(got, home.join("myws"));
        assert!(
            got.is_absolute(),
            "expanded ~ must produce an absolute path, got {got:?}"
        );
    }

    #[test]
    fn workspace_never_returns_dot_or_cwd_when_unset() {
        // Regression guard: an earlier implementation called
        // `.to_str().unwrap_or(".")` on the fallback PathBuf — a path with
        // non-UTF-8 bytes would silently degrade to "." (the gateway's CWD)
        // and let every file tool escape the workspace. The helper must
        // never produce "." or a relative path.
        let base = std::path::Path::new("/some/abs/base");
        let got = resolve_default_workspace(None, None, base);
        assert_ne!(got, std::path::PathBuf::from("."));
        assert!(
            got.is_absolute(),
            "default workspace must be absolute, got {got:?}"
        );
    }

    #[test]
    fn explicit_zero_max_tokens_omits_wire_cap() {
        let got = resolve_request_max_tokens(Some(0), None, None, "doubao", "doubao-seed-2.0-pro");
        assert_eq!(got, None);
    }

    #[test]
    fn skill_list_filters_and_paginates_results() {
        let mut skills = SkillRegistry::new();
        skills.insert(crate::skill::SkillManifest {
            name: "douyin-publish".to_owned(),
            description: Some("Publish videos to Douyin".to_owned()),
            version: None,
            requires_rsclaw: None,
            tools: vec![],
            extra: Default::default(),
            dir: Default::default(),
            prompt: String::new(),
        });
        skills.insert(crate::skill::SkillManifest {
            name: "weather".to_owned(),
            description: Some("Forecast lookup".to_owned()),
            version: None,
            requires_rsclaw: None,
            tools: vec![],
            extra: Default::default(),
            dir: Default::default(),
            prompt: String::new(),
        });
        skills.insert(crate::skill::SkillManifest {
            name: "douyin-comments".to_owned(),
            description: Some("Read Douyin comments".to_owned()),
            version: None,
            requires_rsclaw: None,
            tools: vec![],
            extra: Default::default(),
            dir: Default::default(),
            prompt: String::new(),
        });
        let result = paginate_skill_list(
            skills.all(),
            &json!({"query": "douyin", "limit": 1, "offset": 1}),
        );

        assert_eq!(result["count"], 3);
        assert_eq!(result["matched"], 2);
        assert_eq!(result["offset"], 1);
        assert_eq!(result["limit"], 1);
        assert_eq!(result["has_more"], false);
        assert_eq!(result["next_offset"], Value::Null);
        let skills = result["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0]["name"], "douyin-publish");
    }

    #[test]
    fn resolve_or_create_collection_creates_then_reuses_by_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kb = crate::kb::KnowledgeService::open(tmp.path().join("kb")).unwrap();

        // First call creates it.
        let (id1, name1, created1) = resolve_or_create_collection(&kb, "会议记录", None).unwrap();
        assert!(created1);
        assert_eq!(name1, "会议记录");

        // Second call reuses the same collection (no duplicate).
        let (id2, _n, created2) = resolve_or_create_collection(&kb, "会议记录", None).unwrap();
        assert!(!created2);
        assert_eq!(id1, id2);

        // Case-insensitive match for ASCII names.
        let (_, _, created3) = resolve_or_create_collection(&kb, "notes", None).unwrap();
        let (_, _, created4) = resolve_or_create_collection(&kb, "NOTES", None).unwrap();
        assert!(created3 && !created4);
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_owned()),
            rsclaw_hidden: None,
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
    fn default_memory_scope_keeps_user_turns_in_agent_scope() {
        assert_eq!(default_memory_scope("main", "chat"), "agent:main");
        assert_eq!(default_memory_scope("main", "a2a"), "agent:main");
        assert_eq!(default_memory_scope("main", "cron"), "agent:main:cron");
        assert_eq!(
            default_memory_scope("main", "heartbeat"),
            "agent:main:heartbeat"
        );
    }

    #[test]
    fn normalize_memory_scope_accepts_legacy_bare_agent_id() {
        assert_eq!(normalize_memory_scope("main", "main"), "agent:main");
        assert_eq!(normalize_memory_scope("agent:main", "main"), "agent:main");
        assert_eq!(normalize_memory_scope("global", "main"), "global");
    }

    #[test]
    fn recall_bundle_from_docs_is_raw_context_with_metadata() {
        let docs = vec![
            MemoryDoc {
                id: "note-1".into(),
                scope: "agent:main".into(),
                kind: "note".into(),
                text: "在吗".into(),
                vector: vec![],
                created_at: 0,
                accessed_at: 0,
                access_count: 0,
                importance: 0.1,
                tier: MemDocTier::Peripheral,
                abstract_text: None,
                overview_text: None,
                tags: vec![],
                pinned: false,
            },
            MemoryDoc {
                id: "entity-1".into(),
                scope: "agent:main".into(),
                kind: "entity".into(),
                text: "用户手机号: 13900001234".into(),
                vector: vec![],
                created_at: 0,
                accessed_at: 0,
                access_count: 0,
                importance: 0.95,
                tier: MemDocTier::Core,
                abstract_text: None,
                overview_text: None,
                tags: vec!["pinned".into()],
                pinned: true,
            },
        ];

        let bundle = recall_bundle_from_docs(docs, 1200, "trace-1").expect("bundle");
        assert_eq!(bundle.context, "- 用户手机号: 13900001234");
        assert!(!bundle.context.contains("<recall>"));
        assert_eq!(bundle.metadata.doc_ids, vec!["entity-1"]);
        assert_eq!(bundle.metadata.mode, "committed");
        assert_eq!(bundle.metadata.format, "xml");
        assert_eq!(bundle.metadata.source, "server");
        assert_eq!(bundle.metadata.trace_id.as_deref(), Some("trace-1"));
        assert_eq!(bundle.metadata.max_tokens, Some(1200));
        assert!(bundle.metadata.hash.starts_with("sha256:"));
        assert!(!bundle.metadata.truncated);
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
            rsclaw_hidden: None,
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
            "memory",
            "session",
            "agent",
            "channel",
            "read_file",
            "write_file",
            "shell",
        ] {
            assert!(
                names.contains(expected),
                "expected built-in tool `{expected}` in tool list, got: {names:?}"
            );
        }
    }

    // ------------------------------------------------------------------
    // is_internal_session vs is_minimal_context_session
    // ------------------------------------------------------------------

    #[test]
    fn is_internal_session_classifies_ephemeral_prefixes() {
        assert!(is_internal_session("heartbeat:tick-42"));
        assert!(is_internal_session("cron:morning-briefing"));
        assert!(is_internal_session("system:bootstrap"));
        assert!(!is_internal_session("agent:main:telegram:direct:u1"));
        assert!(!is_internal_session("hook:abcd"));
        assert!(!is_internal_session("session:my-named"));
    }

    #[test]
    fn intermediate_notification_text_rejects_whitespace_only_text() {
        assert_eq!(intermediate_notification_text("\n"), None);
        assert_eq!(intermediate_notification_text(" \t\n"), None);
    }

    #[test]
    fn intermediate_notification_text_preserves_real_text_without_mutating_source() {
        let source = "\n正在查看屏幕。\n";
        assert_eq!(
            intermediate_notification_text(source),
            Some("正在查看屏幕。")
        );
        assert_eq!(source, "\n正在查看屏幕。\n");
    }

    #[test]
    fn is_minimal_context_session_excludes_cron() {
        // Heartbeat / system: minimal prompt + memory-only tool set.
        assert!(is_minimal_context_session("heartbeat:tick-42"));
        assert!(is_minimal_context_session("system:bootstrap"));
        // Cron-fired agentTurn must run with the full agent context, even
        // though the session is ephemeral. Regression guard for the
        // "HEARTBEAT_OK" reply bug where cron jobs got the minimal prompt.
        assert!(!is_minimal_context_session("cron:morning-briefing"));
        assert!(!is_minimal_context_session("agent:main:telegram:direct:u1"));
    }

    // ------------------------------------------------------------------
    // model_supports_image_input — schema-driven vision-capability lookup
    // ------------------------------------------------------------------

    fn build_config_with_models(
        provider_name: &str,
        models: Vec<crate::config::schema::ModelDef>,
    ) -> crate::config::schema::Config {
        use crate::config::schema::{ApiFormat, Config, ModelsConfig, ProviderConfig};
        let pc = ProviderConfig {
            base_url: None,
            api_key: None,
            api: Some(ApiFormat::OpenAiCompletions),
            models: Some(models),
            enabled: Some(true),
            user_agent: None,
            prefix_id: None,
            compact_timeout_secs: None,
        };
        let mut providers = std::collections::HashMap::new();
        providers.insert(provider_name.to_owned(), pc);
        Config {
            models: Some(ModelsConfig {
                mode: None,
                providers,
            }),
            ..Config::default()
        }
    }

    fn model_def(
        id: &str,
        inputs: Option<Vec<crate::config::schema::InputType>>,
    ) -> crate::config::schema::ModelDef {
        crate::config::schema::ModelDef {
            id: id.to_owned(),
            name: None,
            reasoning: None,
            input: inputs,
            cost: None,
            context_window: None,
            max_tokens: None,
            enabled: None,
        }
    }

    #[test]
    fn model_supports_image_input_explicit_image() {
        use crate::config::schema::InputType;
        let cfg = build_config_with_models(
            "kimi",
            vec![model_def(
                "kimi-for-coding",
                Some(vec![InputType::Text, InputType::Image]),
            )],
        );
        // Both qualified and unqualified lookups resolve.
        assert_eq!(
            model_supports_image_input(&cfg, "kimi/kimi-for-coding"),
            Some(true)
        );
        assert_eq!(
            model_supports_image_input(&cfg, "kimi-for-coding"),
            Some(true)
        );
    }

    #[test]
    fn model_supports_image_input_text_only() {
        use crate::config::schema::InputType;
        let cfg = build_config_with_models(
            "deepseek",
            vec![model_def("deepseek-chat", Some(vec![InputType::Text]))],
        );
        assert_eq!(
            model_supports_image_input(&cfg, "deepseek/deepseek-chat"),
            Some(false)
        );
    }

    #[test]
    fn model_supports_image_input_no_input_field_returns_none() {
        let cfg = build_config_with_models("kimi", vec![model_def("kimi-for-coding", None)]);
        // input field absent → caller should fall back to blocklist.
        assert_eq!(
            model_supports_image_input(&cfg, "kimi/kimi-for-coding"),
            None
        );
    }

    #[test]
    fn model_supports_image_input_unknown_model_returns_none() {
        use crate::config::schema::InputType;
        let cfg = build_config_with_models(
            "kimi",
            vec![model_def("kimi-for-coding", Some(vec![InputType::Image]))],
        );
        assert_eq!(model_supports_image_input(&cfg, "openai/gpt-4"), None);
    }

    // ------------------------------------------------------------------
    // is_known_vision_model — built-in allow-list
    // ------------------------------------------------------------------

    #[test]
    fn is_known_vision_model_kimi_family() {
        // kimi-for-coding ships vision tuning.
        assert!(is_known_vision_model("kimi/kimi-for-coding"));
        assert!(is_known_vision_model("kimi-for-coding"));
        // K2.5+ series is multimodal; older K2.x (K2.0..=K2.4) is not.
        assert!(is_known_vision_model("kimi/kimi-k2.5"));
        assert!(is_known_vision_model("kimi/kimi-k2.6-preview"));
        assert!(is_known_vision_model("kimi/kimi-k2.7"));
        // Pre-2.5 must NOT match.
        assert!(!is_known_vision_model("kimi/kimi-k2.0"));
        assert!(!is_known_vision_model("kimi/kimi-k1"));
    }

    #[test]
    fn is_known_vision_model_major_vlms() {
        for name in [
            // International
            "openai/gpt-4o",
            "openai/gpt-4-vision-preview",
            "openai/gpt-5",
            "anthropic/claude-3-opus",
            "anthropic/claude-sonnet-4-5",
            "anthropic/claude-4-7",
            "google/gemini-1.5-pro",
            "google/gemini-3-ultra",
            "google/gemma-3-27b-it",
            "google/gemma-4-9b",
            "google/paligemma-3b-mix",
            "meta/llama-3.2-90b-vision-instruct",
            "meta/llama-4-scout-17b",
            "mistral/pixtral-12b",
            "mistral/mistral-small-3.1-24b",
            "cohere/aya-vision-32b",
            "xai/grok-3",
            "xai/grok-4-fast",
            // Chinese — ByteDance / Alibaba / Moonshot / Zhipu / Baidu / 01 / Baichuan / DeepSeek
            // / Tencent / MiniMax / StepFun
            "doubao/doubao-seed-1.5-vision-pro",
            "doubao/doubao-seed-1.6-vision-thinking",
            // Doubao Seed 2+ — entire 2.x / 3.x / ... subtree is multimodal
            "doubao/doubao-seed-2.0-pro",
            "doubao/doubao-seed-2.0-lite",
            "doubao/doubao-seed-2.0-code",
            "doubao/doubao-seed-2.0-vision",
            "doubao/doubao-seed-2.0-flash",
            "doubao/doubao-seed-2.5-pro", // future minor
            "doubao/doubao-seed-3.0-pro", // future major (auto-covered)
            "doubao/doubao-seed-4-omni",
            "doubao/doubao-vision",
            "doubao/seedream",
            "qwen/qwen-vl-plus",
            "qwen/qwen2.5-vl-72b",
            "qwen/qwen3-vl-30b",
            "qwen/qwen3.5-instruct",
            "qwen/qwen-3.6-pro",
            "qwen/qvq-72b-preview",
            "kimi/kimi-for-coding",
            "kimi/kimi-k2.5",
            "kimi/kimi-k2.6-preview",
            "kimi/kimi-vl-thinking",
            "zhipu/glm-4v-9b",
            "zhipu/glm-4.5v",
            "zhipu/cogagent-9b",
            "baidu/ernie-4.5-vl-424b",
            "baidu/ernie-5-pro",
            "sensetime/sensenova-v6-pro",
            "01-ai/yi-vl-34b",
            "baichuan/baichuan-omni-1.5",
            "deepseek/deepseek-vl2",
            "deepseek/janus-pro-7b",
            "tencent/hunyuan-vision",
            "minimax/minimax-vl-01",
            "stepfun/step-1o-vision-32k",
            "stepfun/step-3",
            // Open-source
            "liuhaotian/llava-1.6-34b",
            "opengvlab/internvl3-78b",
            "openbmb/minicpm-v-2.6",
            "microsoft/phi-3-vision-128k",
            "microsoft/florence-2-large",
            "huggingfaceh4/idefics3-8b",
            "huggingfaceh4/smolvlm-instruct",
            "nvidia/nvila-15b",
            // GUI-agent VLMs
            "bytedance/ui-tars-1.5-7b",
            "bytedance/ui-tars-2",
            "showui-2b",
            "os-atlas-pro-7b",
            // Universal suffix matchers
            "anything-with-vision-suffix",
            "weird-foo-omni",
        ] {
            assert!(is_known_vision_model(name), "should match: {name}");
        }
    }

    #[test]
    fn is_known_vision_model_text_only_returns_false() {
        for name in [
            // OpenAI text-only
            "openai/gpt-3.5-turbo",
            "openai/gpt-4", // bare GPT-4 base is text-only
            "openai/text-davinci-003",
            // Anthropic legacy
            "anthropic/claude-2.1",
            "anthropic/claude-instant-1",
            // DeepSeek non-VL
            "deepseek/deepseek-chat",
            "deepseek/deepseek-reasoner",
            "deepseek/deepseek-coder",
            "deepseek/deepseek-v3",
            // Doubao text-only
            "doubao/doubao-seed-1.6", // text variant; only -vision suffix is multimodal
            "doubao/doubao-pro-256k",
            "doubao/doubao-lite",
            // Qwen text-only (pre-3.5)
            "qwen/qwen-turbo",
            "qwen/qwen-max",
            "qwen/qwen-plus",
            "qwen/qwen3.0",
            "qwen/qwen3.4",
            "qwen/qwen-3.4-instruct",
            "qwen/qwen3-coder", // coder is text-only
            // Pre-3 Gemma
            "google/gemma-2-9b",
            "google/gemma-1-7b",
            // Llama text-only
            "meta/llama-3-70b",
            "meta/llama-3.1-405b",
            "meta/llama-3.2-3b", // small Llama 3.2 are text
            // Mistral text-only
            "mistral/mistral-7b-instruct",
            "mistral/mixtral-8x7b",
            "mistral/codestral-22b",
            "mistral/mistral-large-2411",
            // Kimi pre-2.5
            "kimi/kimi-k1",
            "kimi/kimi-k2.0",
            "kimi/kimi-k2.4",
            "kimi/moonshot-v1-128k", // base v1 is text without -vision
            // Zhipu text-only (no v suffix)
            "zhipu/glm-4-flash",
            "zhipu/glm-4.5",
            "zhipu/glm-5", // bare GLM-5 (the VL variant is glm-5v)
            // Baidu text-only
            "baidu/ernie-3.5-128k",
            "baidu/ernie-4.0-turbo",
            "baidu/ernie-speed",
            // Yi text-only
            "01-ai/yi-large",
            "01-ai/yi-lightning",
            // Baichuan text-only
            "baichuan/baichuan2-13b",
            "baichuan/baichuan4",
            // Hunyuan text-only
            "tencent/hunyuan-large",
            "tencent/hunyuan-t1",
            // MiniMax text-only — including base M2 / M2.5 / M2.7
            // (despite "native multimodal" marketing, third-party
            // testing confirms text-only input).
            "minimax/abab6.5-chat",
            "minimax/minimax-m1",
            "minimax/minimax-m2",
            "minimax/minimax-m2.5",
            "minimax/minimax-m2.7",
            "minimax/minimax-m3-base",
            // StepFun text-only
            "stepfun/step-1-128k",
            "stepfun/step-2-mini",
            // SmolLM (NOT SmolVLM)
            "huggingfaceh4/smollm-1.7b",
            "huggingfaceh4/smollm2-1.7b",
            // MiniCPM bare (NOT minicpm-v)
            "openbmb/minicpm-2b",
            "openbmb/minicpm3-4b",
            // Phi text-only
            "microsoft/phi-3-mini-4k",
            "microsoft/phi-4", // bare phi-4 is text; phi-4-multimodal is vision
            // Generic / unknown model — defaults to text-only.
            "some-new-llm/v1",
            "future-vendor/futurelm-2030",
        ] {
            assert!(
                !is_known_vision_model(name),
                "should NOT match (false positive): {name}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // plugin_search pure-helper tests (Task 1)
    // ---------------------------------------------------------------------

    fn pti(plugin: &str, tool: &str, desc: &str) -> PluginToolInfo {
        PluginToolInfo {
            plugin: plugin.to_owned(),
            runtime: "wasm",
            tool: tool.to_owned(),
            description: desc.to_owned(),
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn search_empty_query_with_plugin_lists_all_alphabetical() {
        let tools = vec![
            pti("demo", "zeta", ""),
            pti("demo", "alpha", ""),
            pti("demo", "mid", ""),
            pti("other", "noise", ""),
        ];
        let result =
            AgentRuntime::search_plugin_tools_pure(tools, &json!({"plugin": "demo", "query": ""}));
        assert_eq!(result["mode"], "list");
        assert_eq!(result["total"], 3);
        let names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["tool"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
        assert!(result.get("error").is_none());
    }

    #[test]
    fn search_empty_query_no_plugin_errors() {
        let result = AgentRuntime::search_plugin_tools_pure(vec![], &json!({"query": ""}));
        assert!(result.get("error").is_some());
    }

    #[test]
    fn search_supports_offset_pagination() {
        let tools = (0..5)
            .map(|i| pti("demo", &format!("t{i}"), ""))
            .collect::<Vec<_>>();
        let result = AgentRuntime::search_plugin_tools_pure(
            tools,
            &json!({"plugin": "demo", "query": "", "offset": 2, "limit": 2}),
        );
        let names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["tool"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["t2", "t3"]);
        assert_eq!(result["next_offset"], json!(4));
    }

    #[test]
    fn search_last_page_has_null_next_offset() {
        let tools = (0..3)
            .map(|i| pti("demo", &format!("t{i}"), ""))
            .collect::<Vec<_>>();
        let result = AgentRuntime::search_plugin_tools_pure(
            tools,
            &json!({"plugin": "demo", "query": "", "offset": 2, "limit": 5}),
        );
        assert_eq!(result["tools"].as_array().unwrap().len(), 1);
        assert_eq!(result["next_offset"], Value::Null);
    }

    // ---------------------------------------------------------------------
    // PluginOverride resolver tests (Task 2)
    // ---------------------------------------------------------------------

    // render_active_plugin_tools_text orchestration is covered by
    // integration testing via the /plugin slash command. Unit-testing it
    // requires constructing a real WasmPlugin (Engine + Component + Linker),
    // which is infeasible in a lib test. The logic that *can* fail in
    // isolation — override resolution — is covered by the resolver tests
    // below.

    #[test]
    fn resolve_inject_returns_none_when_no_override() {
        let overrides: std::collections::HashMap<String, PluginOverride> = Default::default();
        let r = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
        assert_eq!(r, PluginInjectResolution::None);
    }

    #[test]
    fn resolve_inject_returns_names_when_explicit() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "douyin".to_owned(),
            PluginOverride {
                inject: vec!["publish".into(), "list".into()],
                ..Default::default()
            },
        );
        let r = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
        assert_eq!(
            r,
            PluginInjectResolution::Names(vec!["publish".into(), "list".into()])
        );
    }

    #[test]
    fn resolve_inject_returns_all_when_inject_all() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "douyin".to_owned(),
            PluginOverride {
                inject_all: true,
                ..Default::default()
            },
        );
        let r = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
        assert_eq!(r, PluginInjectResolution::All);
    }

    #[test]
    fn resolve_inject_returns_none_when_disabled() {
        // disabled wins over inject / inject_all.
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "douyin".to_owned(),
            PluginOverride {
                disabled: true,
                inject_all: true,
                inject: vec!["publish".into()],
                ..Default::default()
            },
        );
        let r = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
        assert_eq!(r, PluginInjectResolution::None);
    }

    // ---------------------------------------------------------------------
    // Qualified tool name parsing (`<plugin>__<tool>` and legacy forms)
    // ---------------------------------------------------------------------

    #[test]
    fn parse_qualified_tool_canonical_double_underscore() {
        let r = super::parse_qualified_tool("douyin__publish");
        assert_eq!(r, Some(("douyin".into(), "publish".into())));
    }

    #[test]
    fn parse_qualified_tool_legacy_dot_separator() {
        // Old `model.plugin_tools` configs used the dotted form;
        // accept it for backward compat.
        let r = super::parse_qualified_tool("douyin.publish");
        assert_eq!(r, Some(("douyin".into(), "publish".into())));
    }

    #[test]
    fn parse_qualified_tool_legacy_slash_separator() {
        // Operators muscle-memory from skill paths sometimes use /.
        let r = super::parse_qualified_tool("douyin/publish");
        assert_eq!(r, Some(("douyin".into(), "publish".into())));
    }

    #[test]
    fn parse_qualified_tool_double_underscore_wins_over_dot() {
        // When both separators are present in a tool name we prefer
        // the canonical form so a tool literally named `foo.bar`
        // inside plugin `p` (`p__foo.bar`) resolves correctly.
        let r = super::parse_qualified_tool("p__foo.bar");
        assert_eq!(r, Some(("p".into(), "foo.bar".into())));
    }

    #[test]
    fn parse_qualified_tool_returns_none_without_separator() {
        assert_eq!(super::parse_qualified_tool("publish"), None);
        assert_eq!(super::parse_qualified_tool(""), None);
    }

    #[test]
    fn bucket_qualified_names_groups_by_plugin() {
        let entries = vec![
            "douyin__publish".to_owned(),
            "douyin.list_my_videos".to_owned(), // legacy form, same plugin
            "jimeng__image_txt2img".to_owned(),
            "garbage_no_separator".to_owned(), // dropped silently
        ];
        let buckets = super::bucket_qualified_names(&entries);
        assert_eq!(buckets.len(), 2);
        let douyin = buckets.get("douyin").expect("douyin bucket present");
        assert_eq!(douyin.len(), 2);
        assert!(douyin.contains("publish"));
        assert!(douyin.contains("list_my_videos"));
        let jimeng = buckets.get("jimeng").expect("jimeng bucket present");
        assert!(jimeng.contains("image_txt2img"));
    }

    #[test]
    fn plugin_user_tool_selection_wire_name_uses_double_underscore() {
        let sel = super::PluginUserToolSelection {
            plugin_name: "douyin".into(),
            tool_name: "publish".into(),
            description: String::new(),
            input_schema: json!({}),
        };
        assert_eq!(sel.wire_name(), "douyin__publish");
    }

    #[test]
    fn search_query_mode_scores_and_paginates() {
        let tools = vec![
            pti("demo", "publish_video", "Publish a video"),
            pti("demo", "edit_video", "Edit a video"),
            pti("demo", "add_account", "Manage account"),
        ];
        let result =
            AgentRuntime::search_plugin_tools_pure(tools, &json!({"query": "video", "limit": 5}));
        assert_eq!(result["mode"], "search");
        let names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["tool"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"publish_video"));
        assert!(names.contains(&"edit_video"));
    }
}
