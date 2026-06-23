//! Agent registry — maps agent IDs to live `AgentHandle`s.
//!
//! The registry is built at gateway startup from `RuntimeConfig.agents`
//! and is shared (via `Arc`) across all concurrent requests.
//!
//! Internal storage uses `std::sync::RwLock` so that `insert_handle` can be
//! called from outside (e.g. `AgentSpawner`) without requiring `&mut self`.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize},
    },
    time::Instant,
};

use anyhow::Result;
use tokio::sync::{RwLock, mpsc};

use rsclaw_config::{runtime::RuntimeConfig, schema::AgentEntry};

/// Default maximum concurrent turns per agent when not configured.
const DEFAULT_MAX_CONCURRENT: u32 = 4;

// ---------------------------------------------------------------------------
// AgentHandle
// ---------------------------------------------------------------------------

// `AgentKind` lifted to `rsclaw-types` (crate-split); re-exported here so
// existing `crate::registry::AgentKind` / `crate::AgentKind`
// paths keep resolving.
pub use rsclaw_types::AgentKind;

/// A lightweight handle to a running agent.
///
/// The actual agent loop lives in a tokio task; communication happens
/// through channels. This struct is cheaply `Clone`-able.
#[derive(Clone)]
pub struct AgentHandle {
    pub id: String,
    pub kind: AgentKind,
    pub config: AgentEntry,
    /// Sender to the agent's message queue.
    pub tx: mpsc::Sender<AgentMessage>,
    /// Limits concurrent turns for this agent.
    pub concurrency: Arc<tokio::sync::Semaphore>,
    /// Shared live status for /btw parallel queries.
    pub live_status: Arc<RwLock<crate::runtime::LiveStatus>>,
    /// Provider registry for direct LLM calls (used by /btw bypass).
    pub providers: Arc<rsclaw_provider::registry::ProviderRegistry>,
    /// Per-session abort flags: session_key -> atomic abort flag.
    /// Uses std::sync::RwLock (not tokio) so it can be accessed in Drop impls.
    ///
    /// Cooperative: polled at stream-loop and tool-dispatch boundaries. Useless
    /// when the turn is blocked inside a non-yielding `.await` (e.g. a hung LLM
    /// SSE connection) — for that, [`Self::cancel_tokens`] provides a hard
    /// cancel that drops the in-flight future regardless of where it parked.
    pub abort_flags: Arc<std::sync::RwLock<HashMap<String, Arc<AtomicBool>>>>,
    /// Per-session hard-cancel tokens: session_key -> CancellationToken.
    /// Minted per turn by the runtime worker for non-A2A turns and registered
    /// here so `chat.abort` can fire `.cancel()`, which the worker's
    /// `tokio::select!` observes immediately — even when the turn is stuck on
    /// a stalled await. Without this a hung turn would block the whole
    /// single-threaded queue until the 30-minute turn timeout fired.
    pub cancel_tokens:
        Arc<std::sync::RwLock<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// Per-session plugin activation overrides:
    /// `session_key → { plugin_name → PluginOverride }`. Mutated by the
    /// `/plugin` slash command (host-side, never enters conversation
    /// history). Resolved at system-prompt build time to render an
    /// "## Active Plugin Tools" block into user_system, exposing the
    /// full input_schema for the selected plugin tools so small models
    /// can call them directly without the plugin_search → plugin_invoke
    /// two-step. The block lives in user_system (not in tools[]), so the
    /// shared prefix KV cache stays intact across overrides.
    pub plugin_overrides: Arc<
        std::sync::RwLock<HashMap<String, HashMap<String, crate::runtime::PluginOverride>>>,
    >,
    /// Loaded WASM plugins shared with queue-bypassing slash handlers.
    pub wasm_plugins: Arc<std::sync::RwLock<Arc<Vec<rsclaw_plugin::WasmPlugin>>>>,
    /// Outbound notification sender shared with queue-bypassing slash handlers.
    pub notification_tx:
        Arc<std::sync::RwLock<Option<tokio::sync::broadcast::Sender<rsclaw_channel::OutboundMessage>>>>,
    /// Per-session re-enabled cold tools: `session_key → {tool_name}`.
    /// Cold builtin tools (near-zero observed usage) are deferred behind the
    /// `request_tool` stub on non-rsclaw providers; calling the stub records
    /// the name here and the runtime splices the real ToolDef back into the
    /// live tool list (same turn) and every later turn of the session.
    pub cold_enabled: Arc<std::sync::RwLock<HashMap<String, std::collections::HashSet<String>>>>,
    /// When this agent handle was created (for /status uptime).
    pub started_at: Instant,
    /// Number of active sessions (updated after each turn for /status).
    pub session_count: Arc<AtomicUsize>,
    /// Per-session context token stats, updated by normal conversation LLM
    /// calls only.
    pub session_tokens: Arc<std::sync::RwLock<HashMap<String, SessionTokens>>>,
    /// Total context tokens of the most recent turn (sys + tools + msgs +
    /// scratchpad). Updated by runtime for /status so the number matches
    /// what the LLM actually saw.
    pub last_ctx_tokens: Arc<AtomicUsize>,
    /// System prompt tokens of the most recent LLM call.
    pub last_sys_tokens: Arc<AtomicUsize>,
    /// Tool-definition tokens of the most recent LLM call.
    pub last_tools_tokens: Arc<AtomicUsize>,
    /// Message-history tokens of the most recent LLM call.
    pub last_msg_tokens: Arc<AtomicUsize>,
    /// Signal to clear all sessions (set by /clear bypass, consumed by
    /// runtime).
    pub clear_signal: Arc<AtomicBool>,
    /// Signal to start a new session (set by /new bypass, consumed by runtime).
    /// Unlike clear_signal, this increments the archive generation.
    pub new_session_signal: Arc<AtomicBool>,
    /// Context window in tokens for this agent's primary model. Resolved
    /// at construction time by looking up the model name (e.g.
    /// `brain/brain-model`) in the provider config's `contextWindow`
    /// field, with `agent.model.contextTokens` overriding when set.
    /// Used by /status; 0 when nothing matched and the caller should
    /// fall back to its own default.
    pub context_window: usize,
    /// Effective primary model name, resolved at construction via the same
    /// chain as [`crate::runtime::resolve_primary_model_for`] (per-agent
    /// `model.primary` → `agents.defaults.model.primary` → rsclaw fallback).
    /// `/model` and `/status` display this so they show the real model
    /// instead of the literal "default" when only `agents.defaults` is set.
    pub effective_model: String,
}

/// Per-session context token statistics.
#[derive(Debug, Clone, Default)]
pub struct SessionTokens {
    /// System prompt tokens.
    pub sys: usize,
    /// Tool definition tokens.
    pub tools: usize,
    /// Message history tokens.
    pub msgs: usize,
    /// Total context tokens (sys + tools + msgs).
    pub total: usize,
}

impl AgentHandle {
    /// Replace the WASM plugin snapshot visible to slash handlers.
    pub fn set_wasm_plugins(&self, plugins: Arc<Vec<rsclaw_plugin::WasmPlugin>>) {
        if let Ok(mut g) = self.wasm_plugins.write() {
            *g = plugins;
        }
    }

    /// Return the current WASM plugin snapshot.
    pub fn wasm_plugins_snapshot(&self) -> Arc<Vec<rsclaw_plugin::WasmPlugin>> {
        self.wasm_plugins
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|_| Arc::new(Vec::new()))
    }

    /// Set the outbound notification sender visible to slash handlers.
    pub fn set_notification_tx(
        &self,
        tx: tokio::sync::broadcast::Sender<rsclaw_channel::OutboundMessage>,
    ) {
        if let Ok(mut g) = self.notification_tx.write() {
            *g = Some(tx);
        }
    }

    /// Return the outbound notification sender, when the gateway has one.
    pub fn notification_tx(
        &self,
    ) -> Option<tokio::sync::broadcast::Sender<rsclaw_channel::OutboundMessage>> {
        self.notification_tx.read().ok().and_then(|g| g.clone())
    }

    /// Record context token stats for a conversation session.
    pub fn update_session_tokens(&self, session_key: &str, tokens: SessionTokens) {
        if let Ok(mut map) = self.session_tokens.write() {
            map.insert(session_key.to_owned(), tokens);
        }
    }

    /// Remove token stats when a session is cleared.
    pub fn remove_session_tokens(&self, session_key: &str) {
        if let Ok(mut map) = self.session_tokens.write() {
            map.remove(session_key);
        }
    }

    /// Unified status string used by all channels (desktop WS, feishu,
    /// telegram, etc.).
    pub fn format_status(&self) -> String {
        use crate::prompt_builder::format_duration;

        let model = self.effective_model.as_str();
        let sessions = self
            .session_count
            .load(std::sync::atomic::Ordering::Relaxed);
        let uptime = format_duration(self.started_at.elapsed());
        let os = if cfg!(target_os = "macos") {
            "macOS"
        } else if cfg!(target_os = "linux") {
            if std::env::var("ANDROID_ROOT").is_ok() {
                "Android"
            } else {
                "Linux"
            }
        } else if cfg!(target_os = "windows") {
            "Windows"
        } else if cfg!(target_os = "ios") {
            "iOS"
        } else {
            "Unknown"
        };
        // Resolution chain (already done at AgentHandle construction):
        //   1. agent.model.contextTokens
        //   2. agents.defaults.contextTokens
        //   3. fallback 128000 below when both are unset (handle.context_window == 0).
        let ctx_limit = if self.context_window > 0 {
            self.context_window
        } else {
            128000
        };

        let mut ctx_lines = String::new();
        if let Ok(map) = self.session_tokens.read() {
            if map.is_empty() {
                ctx_lines.push_str("Context: (no sessions)\n");
            } else {
                for (key, t) in map.iter() {
                    let short_key = if key.len() > 20 { &key[..20] } else { key };
                    let pct = if ctx_limit > 0 {
                        t.total * 100 / ctx_limit
                    } else {
                        0
                    };
                    ctx_lines.push_str(&format!(
                        "Context [{short_key}]:\n\
                         \u{A0} system  ~{:.1}k\n\
                         \u{A0} tools   ~{:.1}k\n\
                         \u{A0} msgs    ~{:.1}k\n\
                         \u{A0} total   ~{:.1}k / {:.0}k ({}%)\n",
                        t.sys as f64 / 1000.0,
                        t.tools as f64 / 1000.0,
                        t.msgs as f64 / 1000.0,
                        t.total as f64 / 1000.0,
                        ctx_limit as f64 / 1000.0,
                        pct,
                    ));
                }
            }
        }

        // Sticky cap binding snapshot — without this section the user
        // has no way to tell whether `/cap claudecode` is still
        // routing every message to a coding agent, or whether main
        // LLM is back in charge. The cap_live_manager is reached via
        // a process-wide OnceLock so this stays sync and free of
        // tokio::runtime::Handle::current() dependencies.
        let mut sticky_lines = String::new();
        if let Some(mgr) = rsclaw_cap::GLOBAL_CAP_LIVE.get() {
            let snapshot = mgr.snapshot_sticky_blocking();
            if !snapshot.is_empty() {
                sticky_lines.push_str("Sticky cap:\n");
                for (im_key, sid, kind) in &snapshot {
                    // Keep the IM session key short — first 24 chars
                    // is enough to identify it across the common
                    // `agent:main:feishu:direct:<peer>` shape, and
                    // long enough that two peers don't collide.
                    let key_label = if im_key.chars().count() > 32 {
                        let head: String = im_key.chars().take(32).collect();
                        format!("{head}…")
                    } else {
                        im_key.clone()
                    };
                    let sid_short = &sid[..8.min(sid.len())];
                    let native = mgr.agent_session_id_blocking(sid);
                    if let Some(nsid) = native {
                        sticky_lines.push_str(&format!(
                            "\u{A0} {} → {} ({})\n  resume: /cap-resume {} {}\n",
                            key_label,
                            kind.as_str(),
                            sid_short,
                            kind.as_str(),
                            nsid
                        ));
                    } else {
                        sticky_lines.push_str(&format!(
                            "\u{A0} {} → {} ({})\n  resume id: (capturing…)\n",
                            key_label,
                            kind.as_str(),
                            sid_short
                        ));
                    }
                }
            }
        }

        // In-flight tool snapshot — surfaces "search_file running for
        // 8min" so the user can see exactly why their /status reply was
        // delayed. Without this section, a wedged tool is invisible
        // until the operator goes hunting in logs.
        //
        // `live_status` is a tokio::sync::RwLock, so the only way to
        // peek synchronously from this non-async fn is `try_read`. That
        // returns immediately whether the lock is held or not — we
        // accept that a /status fired the same instant another task
        // holds the write side just gets an empty in-flight section
        // (caller can /status again). Vastly cheaper than retrofitting
        // format_status into an async fn touched by 30+ callers.
        let mut in_flight_lines = String::new();
        if let Ok(status) = self.live_status.try_read() {
            if !status.in_flight_tools.is_empty() {
                in_flight_lines.push_str("In-flight tools:\n");
                let now = Instant::now();
                for (tool, started) in &status.in_flight_tools {
                    let secs = now.saturating_duration_since(*started).as_secs();
                    let label = if secs < 60 {
                        format!("{secs}s")
                    } else if secs < 3600 {
                        format!("{}m {}s", secs / 60, secs % 60)
                    } else {
                        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
                    };
                    in_flight_lines.push_str(&format!("\u{A0} {tool} ({label})\n"));
                }
            }
        }

        // Which agent answered this conversation — the channel/account routed
        // here via route_account, so surfacing the handle's id (+ display name)
        // tells the user exactly which agent they're talking to.
        let agent_line = match self.config.name.as_deref().filter(|n| !n.is_empty()) {
            Some(name) if name != self.id => format!("Agent: {} ({name})\n", self.id),
            _ => format!("Agent: {}\n", self.id),
        };

        format!(
            "{agent_line}Gateway: running\nOS: {os}\nModel: {model}\nSessions: {sessions}\n\
             {ctx_lines}\
             {sticky_lines}\
             {in_flight_lines}\
             Uptime: {uptime}\nVersion: rsclaw {}",
            option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
        )
    }

    /// Render the `/model` (`/models`) reply: the effective model plus the
    /// registered provider list. Single source of truth shared by the
    /// gateway fast-preparse path and the agent-queue `__MODELS__` template,
    /// so the two can never drift (they previously rendered "default" vs the
    /// resolved name independently).
    pub fn format_models(&self) -> String {
        let mut lines = vec![format!("Current model: {}", self.effective_model)];
        lines.push(String::new());
        lines.push("Registered providers:".to_owned());
        for name in self.providers.names() {
            lines.push(format!("  {name}"));
        }
        lines.join("\n")
    }
}

// `ImageAttachment` / `FileAttachment` lifted to `rsclaw-types` (crate-split);
// re-exported here so existing `crate::registry::{ImageAttachment,
// FileAttachment}` paths keep resolving.
pub use rsclaw_types::{FileAttachment, ImageAttachment};

/// Per-turn observability + control wires that an A2A caller threads into the
/// runtime. Built from the three optional channels on `AgentMessage` plus the
/// A2A task/context identifiers; mirrored onto `RunContext` so the agent loop
/// can publish progress, observe cancellation, and request user input mid-turn.
///
/// Default-constructed `TurnContext` is the legacy no-op: every method is a
/// no-op or returns false. Non-A2A channels (WebSocket, ACP, runtime
/// self-calls) keep using `TurnContext::default()`.
#[derive(Debug, Clone, Default)]
pub struct TurnContext {
    pub task_id: Option<String>,
    pub context_id: Option<String>,
    pub event_tx: Option<tokio::sync::mpsc::Sender<rsclaw_a2a_types::event::AgentEvent>>,
    pub cancel_token: Option<tokio_util::sync::CancellationToken>,
    pub input_request_tx: Option<tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<String>>>,
    /// Monotonic progress counter bumped once per agent-loop iteration. A
    /// progress-based stuck-turn watchdog (used for daemon agents, which loop
    /// forever so a flat wall-clock limit can't apply) samples it: if it stops
    /// advancing for a while, a tool is genuinely wedged and the turn is
    /// cancelled. `None` for non-A2A/legacy turns (no watchdog wiring).
    pub progress: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl TurnContext {
    /// Bump the progress counter (no-op when unset). Called once per agent-loop
    /// iteration so a progress watchdog can tell "alive but looping" from
    /// "wedged on a non-returning tool".
    pub fn progress_tick(&self) {
        if let Some(p) = &self.progress {
            p.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// True once the A2A caller fired `tasks/cancel` (or the task's cancel
    /// token was tripped for any other reason). Runtime polls this between
    /// iterations of the agent loop and at every tool-dispatch boundary.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.as_ref().is_some_and(|t| t.is_cancelled())
    }

    /// Publish a `TASK_STATE_WORKING` status-update with a human-readable
    /// progress message (e.g. "calling tool memory_search"). Non-A2A turns
    /// (no event_tx) drop the message silently. `try_send` is used so a
    /// full subscriber queue can never stall the agent loop.
    pub fn emit_working(&self, message: &str) {
        let (Some(tx), Some(task_id), Some(context_id)) =
            (&self.event_tx, &self.task_id, &self.context_id)
        else {
            return;
        };
        let _ = tx.try_send(rsclaw_a2a_types::event::AgentEvent::Status {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            state: rsclaw_a2a_types::types::TaskState::Working,
            message: Some(rsclaw_a2a_types::event::text_message(message)),
            final_: false,
        });
    }

    /// Suspend the turn awaiting client input. Publishes `InputRequired`
    /// (or `AuthRequired` if `auth`), registers a `SuspendedTask` resume
    /// handle via `input_request_tx`, and awaits the resume. Returns the
    /// client-supplied text, or `None` if the wire is absent / the
    /// resume channel was dropped (treat as cancellation).
    pub async fn request_input(&self, prompt: &str, auth: bool) -> Option<String> {
        let ireq_tx = self.input_request_tx.as_ref()?;
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel::<String>();
        if ireq_tx.send(resume_tx).await.is_err() {
            return None;
        }
        if let (Some(tx), Some(task_id), Some(context_id)) =
            (&self.event_tx, &self.task_id, &self.context_id)
        {
            let prompt_msg = rsclaw_a2a_types::event::text_message(prompt);
            let ev = if auth {
                rsclaw_a2a_types::event::AgentEvent::AuthRequired {
                    task_id: task_id.clone(),
                    context_id: context_id.clone(),
                    prompt: prompt_msg,
                }
            } else {
                rsclaw_a2a_types::event::AgentEvent::InputRequired {
                    task_id: task_id.clone(),
                    context_id: context_id.clone(),
                    prompt: prompt_msg,
                }
            };
            let _ = tx.try_send(ev);
        }
        resume_rx.await.ok()
    }
}

/// A message delivered to an agent.
#[derive(Debug)]
pub struct AgentMessage {
    /// Session key (determines context isolation).
    pub session_key: String,
    /// The user's text.
    pub text: String,
    /// Channel this message arrived on (e.g. "telegram", "discord").
    pub channel: String,
    /// Multi-account tag (e.g. feishu account name) for outbound routing.
    /// `None` for single-account channels.
    pub account: Option<String>,
    /// Peer / user ID (used for session isolation).
    pub peer_id: String,
    /// Chat/conversation ID for replies (for platforms like Feishu where reply
    /// ID differs from user ID). If empty, defaults to peer_id.
    pub chat_id: String,
    /// One-shot sender for the agent's response.
    pub reply_tx: tokio::sync::oneshot::Sender<AgentReply>,
    /// A2A task id, if the caller is the A2A protocol. Carried on
    /// `TurnContext` so the runtime can tag emitted events with the
    /// right `taskId` / `contextId`.
    pub task_id: Option<String>,
    /// A2A context id (== session key for our impl).
    pub context_id: Option<String>,
    /// Optional streaming event channel. Runtime emits intermediate
    /// `AgentEvent`s (status changes, artifact chunks) here when the caller
    /// (e.g. A2A SendStreamingMessage) wants to observe progress. Legacy
    /// channels pass `None`.
    pub event_tx: Option<tokio::sync::mpsc::Sender<rsclaw_a2a_types::event::AgentEvent>>,
    /// Optional cancellation token. Runtime checks this between LLM/tool
    /// turns; if cancelled, emits TASK_STATE_CANCELED and exits. Legacy
    /// channels pass `None`.
    pub cancel_token: Option<tokio_util::sync::CancellationToken>,
    /// Optional channel for the runtime to register a `SuspendedTask` resume
    /// handle when it needs to ask the caller for more input
    /// (TASK_STATE_INPUT_REQUIRED). Legacy channels pass `None`.
    pub input_request_tx: Option<tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<String>>>,
    /// External tool definitions forwarded from the OAI /v1/chat/completions
    /// caller. These are merged into the agent's tool list for the turn.
    pub extra_tools: Vec<rsclaw_provider::ToolDef>,
    /// Image attachments (vision support). Empty for text-only messages.
    pub images: Vec<ImageAttachment>,
    /// File attachments (raw bytes). Empty for text-only messages.
    pub files: Vec<FileAttachment>,
}

/// Extract `[file:path]` references from user text, read the files from disk,
/// and return (cleaned_text, images, files).
///
/// Image files (jpg/png/gif/webp) become `ImageAttachment`; everything else
/// becomes `FileAttachment`. The `[file:...]` markers are removed from the
/// returned text.
pub fn extract_file_refs(text: &str) -> (String, Vec<ImageAttachment>, Vec<FileAttachment>) {
    use base64::Engine as _;
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[file:([^\]]+)\]").expect("file ref regex")
    });

    let mut images = Vec::new();
    let mut files = Vec::new();

    // Downscale thresholds. Anything over 1 MB of bytes OR with a long
    // edge above 1920 px gets resized + JPEG-re-encoded before going on
    // the wire. The token cost for vision LLMs is driven by pixel
    // dimensions, not file size, so the downscale is quality-neutral for
    // typical screenshots and keeps the payload below every major
    // provider's hard inline limit. On decode failure we keep the original
    // bytes — better to ship a possibly-too-large blob than to drop the
    // user's attachment silently.
    const DOWNSCALE_BYTE_THRESHOLD: usize = 1 * 1024 * 1024;
    const DOWNSCALE_MAX_LONG_EDGE: u32 = 1920;
    const DOWNSCALE_JPEG_QUALITY: u8 = 85;

    for cap in RE.captures_iter(text) {
        let path_str = cap[1].trim();
        let path = std::path::Path::new(path_str);
        let Ok(data) = std::fs::read(path) else {
            tracing::warn!(path = path_str, "file ref: cannot read file");
            continue;
        };

        let lower = path_str.to_lowercase();
        let is_image = lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".png")
            || lower.ends_with(".gif")
            || lower.ends_with(".webp");

        if is_image {
            let orig_mime = if lower.ends_with(".png") {
                "image/png"
            } else if lower.ends_with(".gif") {
                "image/gif"
            } else if lower.ends_with(".webp") {
                "image/webp"
            } else {
                "image/jpeg"
            };
            let orig_len = data.len();
            let (final_bytes, final_mime) = match rsclaw_util::downscale_image_for_vision(
                &data,
                orig_mime,
                DOWNSCALE_BYTE_THRESHOLD,
                DOWNSCALE_MAX_LONG_EDGE,
                DOWNSCALE_JPEG_QUALITY,
            ) {
                Ok((b, m)) => {
                    if b.len() != orig_len {
                        tracing::info!(
                            path = path_str,
                            from = orig_len,
                            to = b.len(),
                            mime = %m,
                            "file ref: image downscaled for vision"
                        );
                    }
                    (b, m)
                }
                Err(e) => {
                    tracing::warn!(
                        path = path_str,
                        error = %e,
                        "file ref: downscale failed, sending original bytes"
                    );
                    // Fall back: reload + keep original (re-read since `data` was moved)
                    let Ok(d) = std::fs::read(path) else {
                        continue;
                    };
                    (d, orig_mime.to_string())
                }
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&final_bytes);
            images.push(ImageAttachment {
                data: format!("data:{final_mime};base64,{b64}"),
                mime_type: final_mime,
                source_path: Some(path_str.to_owned()),
            });
        } else {
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path_str.to_owned());
            let mime = match lower.rsplit('.').next() {
                Some("pdf") => "application/pdf",
                Some("docx") => {
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                }
                Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                Some("pptx") => {
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                }
                Some(
                    "txt" | "md" | "rs" | "py" | "js" | "ts" | "json" | "toml" | "yaml" | "yml",
                ) => "text/plain",
                Some("mp4") => "video/mp4",
                Some("mp3") => "audio/mpeg",
                Some("wav") => "audio/wav",
                _ => "application/octet-stream",
            }
            .to_owned();
            files.push(FileAttachment {
                filename,
                data,
                mime_type: mime,
            });
        }
    }

    let cleaned = RE.replace_all(text, "").to_string();
    let cleaned = cleaned.trim().to_owned();
    (cleaned, images, files)
}

/// Deferred file analysis that the per-user worker should process after
/// sending the initial "analyzing..." reply.
#[derive(Debug)]
pub struct PendingAnalysis {
    /// The extracted text to send to the LLM for analysis.
    pub text: String,
    /// Session key for the follow-up agent call.
    pub session_key: String,
    /// Channel name (e.g. "telegram", "discord").
    pub channel: String,
    /// Peer / user ID.
    pub peer_id: String,
}

/// Reply from an agent.
#[derive(Debug)]
pub struct AgentReply {
    /// Final text output (may be empty if the agent used messaging tools).
    pub text: String,
    /// Whether the agent produced any output at all.
    pub is_empty: bool,
    /// If the LLM called an external (caller-provided) tool, this carries the
    /// tool_calls payload in OpenAI wire format so the OAI handler can return
    /// it.
    pub tool_calls: Option<Vec<serde_json::Value>>,
    /// Images to send (base64 data URIs or file paths).
    pub images: Vec<String>,
    /// File attachments: Vec<(filename, mime_type, file_path_or_url)>.
    pub files: Vec<(String, String, String)>,
    /// If set, the worker should send a follow-up LLM analysis after the
    /// immediate reply.
    pub pending_analysis: Option<PendingAnalysis>,
    /// True when the runtime's outer dispatcher should emit a `done=true`
    /// event_bus frame after this reply.
    ///
    /// Set to `true` for every reply path that bypasses `agent_loop` (preparse
    /// short-circuits, file-attachment short-circuits, `__DIRECT_REPLY__`,
    /// `/btw` side queries, disk-low responses, etc.) since those paths
    /// don't emit a streaming terminator themselves. `agent_loop` emits its
    /// own done frame internally for normal LLM turns and its own
    /// early-return paths (clear_signal, abort, max_iterations, error_streak,
    /// loop-detected), so it sets this to `false`.
    ///
    /// Without this flag, WS subscribers (the desktop chat UI) wait forever
    /// for the terminator and the input freezes.
    pub needs_outer_done_emit: bool,
    /// Why the turn ended. `Ok` = normal success (publish Artifact +
    /// Completed). `Error(_)` = `run_turn` returned `Err` and the gateway
    /// wrapped it as a text reply; A2A consumers should publish `Failed`,
    /// not `Completed`. `Canceled` = `run_turn` returned `Err` because the
    /// A2A cancel_token fired; the CancelTask dispatcher has already
    /// published `Canceled` and closed the bus, so A2A consumers must NOT
    /// republish a terminal status.
    pub outcome: ReplyOutcome,
}

// ReplyOutcome lifted to rsclaw-types (crate-split); re-exported.
pub use rsclaw_types::ReplyOutcome;

// ---------------------------------------------------------------------------
// AgentRegistry
// ---------------------------------------------------------------------------

struct RegistryInner {
    agents: HashMap<String, Arc<AgentHandle>>,
    /// ID of the default agent (first with `default: true`, else first entry).
    default_id: Option<String>,
}

pub struct AgentRegistry {
    inner: std::sync::RwLock<RegistryInner>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self {
            inner: std::sync::RwLock::new(RegistryInner {
                agents: HashMap::new(),
                default_id: None,
            }),
        }
    }
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a registry from `RuntimeConfig`. Does not spawn actual agent loops
    /// (those are started by the gateway in W6+).
    pub fn from_config(cfg: &RuntimeConfig) -> Self {
        let providers = Arc::new(rsclaw_provider::registry::ProviderRegistry::new());
        let (registry, _) = Self::from_config_with_receivers(cfg, providers);
        registry
    }

    /// Build a registry AND return the mpsc receivers for each agent so the
    /// gateway can spawn real runtime tasks.
    pub fn from_config_with_receivers(
        cfg: &RuntimeConfig,
        providers: Arc<rsclaw_provider::registry::ProviderRegistry>,
    ) -> (Self, HashMap<String, mpsc::Receiver<AgentMessage>>) {
        let registry = Self::new();
        let mut receivers = HashMap::new();

        // If agents.list is empty (e.g. openclaw.json with no explicit list),
        // synthesize a default agent from agents.defaults.
        let agent_list = if cfg.agents.list.is_empty() {
            let model = cfg.agents.defaults.model.clone();
            let workspace = cfg.agents.defaults.workspace.clone();
            // Inherit ACP configs from defaults
            let opencode = cfg.agents.defaults.opencode.clone();
            let claudecode = cfg.agents.defaults.claudecode.clone();
            let codex = cfg.agents.defaults.codex.clone();
            vec![rsclaw_config::schema::AgentEntry {
                id: "main".to_owned(),
                daemon: false,
                description: None,
                default: Some(true),
                name: Some("Main Agent".to_owned()),
                workspace,
                model,
                flash_model: None,
                lane: None,
                lane_concurrency: None,
                group_chat: None,
                channels: None,
                commands: None,
                allowed_commands: None,
                opencode,
                claudecode,
                codex,
                agent_dir: None,
                system: None,
                temperature: None,
            }]
        } else {
            cfg.agents.list.clone()
        };

        let max_concurrent = cfg
            .agents
            .defaults
            .max_concurrent
            .unwrap_or(DEFAULT_MAX_CONCURRENT) as usize;

        {
            let mut inner = registry
                .inner
                .write()
                .expect("agent registry lock poisoned");

            for entry in &agent_list {
                let (tx, rx) = mpsc::channel::<AgentMessage>(32);
                let permits = entry
                    .lane_concurrency
                    .map(|n| n as usize)
                    .unwrap_or(max_concurrent);
                let kind = if entry.default == Some(true) {
                    AgentKind::Main
                } else {
                    AgentKind::Named
                };
                // Context window resolution: per-agent model.contextTokens
                // wins, then agents.defaults.contextTokens. 0 = unset, the
                // /status formatter falls back to its own default.
                let context_window = entry
                    .model
                    .as_ref()
                    .and_then(|m| m.context_tokens)
                    .or(cfg.agents.defaults.context_tokens)
                    .unwrap_or(0) as usize;
                let effective_model =
                    crate::runtime::resolve_primary_model_for(entry, &cfg.agents.defaults)
                        .unwrap_or_else(|| "rsclaw/rsclaw-agent-v1".to_owned());
                let handle = Arc::new(AgentHandle {
                    id: entry.id.clone(),
                    kind,
                    config: entry.clone(),
                    tx,
                    concurrency: Arc::new(tokio::sync::Semaphore::new(permits)),
                    live_status: Arc::new(
                        RwLock::new(crate::runtime::LiveStatus::default()),
                    ),
                    providers: Arc::clone(&providers),
                    abort_flags: Arc::new(std::sync::RwLock::new(HashMap::new())),
                    cancel_tokens: Arc::new(std::sync::RwLock::new(HashMap::new())),
                    plugin_overrides: Arc::new(std::sync::RwLock::new(HashMap::new())),
                    wasm_plugins: Arc::new(std::sync::RwLock::new(Arc::new(Vec::new()))),
                    notification_tx: Arc::new(std::sync::RwLock::new(None)),
                    cold_enabled: Arc::new(std::sync::RwLock::new(HashMap::new())),
                    started_at: Instant::now(),
                    session_count: Arc::new(AtomicUsize::new(0)),
                    session_tokens: Arc::new(std::sync::RwLock::new(HashMap::new())),
                    last_ctx_tokens: Arc::new(AtomicUsize::new(0)),
                    last_sys_tokens: Arc::new(AtomicUsize::new(0)),
                    last_tools_tokens: Arc::new(AtomicUsize::new(0)),
                    last_msg_tokens: Arc::new(AtomicUsize::new(0)),
                    clear_signal: Arc::new(AtomicBool::new(false)),
                    new_session_signal: Arc::new(AtomicBool::new(false)),
                    context_window,
                    effective_model,
                });
                inner.agents.insert(entry.id.clone(), handle);
                receivers.insert(entry.id.clone(), rx);

                if entry.default == Some(true) && inner.default_id.is_none() {
                    inner.default_id = Some(entry.id.clone());
                }
            }

            // If no explicit default, use the first agent.
            if inner.default_id.is_none()
                && let Some(id) = inner.agents.keys().next().cloned()
            {
                inner.default_id = Some(id);
            }
        }

        (registry, receivers)
    }

    /// Insert a dynamically spawned agent handle.
    pub fn insert_handle(&self, handle: Arc<AgentHandle>) {
        let mut inner = self.inner.write().expect("agent registry lock poisoned");
        inner.agents.insert(handle.id.clone(), handle);
    }

    /// Remove an agent handle by ID (used for task agents after completion).
    pub fn remove_handle(&self, id: &str) {
        let mut inner = self.inner.write().expect("agent registry lock poisoned");
        inner.agents.remove(id);
    }

    /// Look up an agent by ID.
    pub fn get(&self, id: &str) -> Result<Arc<AgentHandle>> {
        self.inner
            .read()
            .expect("agent registry lock poisoned")
            .agents
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("agent not found: `{id}`"))
    }

    /// Return the default agent.
    pub fn default_agent(&self) -> Result<Arc<AgentHandle>> {
        let inner = self.inner.read().expect("agent registry lock poisoned");
        let id = inner
            .default_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no default agent configured"))?
            .to_owned();
        drop(inner);
        self.get(&id)
    }

    /// Route a message to the correct agent based on channel binding.
    ///
    /// Routing priority:
    ///   1. Agents with `channels` containing this channel name.
    ///   2. Default agent (fallback).
    pub fn route(&self, channel: &str) -> Result<Arc<AgentHandle>> {
        self.route_account(channel, None)
    }

    /// Route a message to the correct agent for a specific channel account.
    ///
    /// Routing priority:
    ///   1. Agents with `channels` containing `"channel:account"` (exact
    ///      match).
    ///   2. Agents with `channels` containing `"channel"` (bare channel match).
    ///   3. Default agent (fallback).
    pub fn route_account(&self, channel: &str, account: Option<&str>) -> Result<Arc<AgentHandle>> {
        let inner = self.inner.read().expect("agent registry lock poisoned");

        // Build the qualified key "channel:account" for exact matching.
        let qualified = account.map(|a| format!("{channel}:{a}"));

        // Find agents explicitly bound to this channel+account.
        // Prefer exact "channel:account" match over bare "channel".
        let mut exact: Vec<Arc<AgentHandle>> = Vec::new();
        let mut bare: Vec<Arc<AgentHandle>> = Vec::new();

        for a in inner.agents.values() {
            let Some(chs) = a.config.channels.as_ref() else {
                continue;
            };
            if let Some(q) = &qualified {
                if chs.iter().any(|c| c == q) {
                    exact.push(Arc::clone(a));
                    continue;
                }
            }
            if chs.iter().any(|c| c == channel) {
                bare.push(Arc::clone(a));
            }
        }

        let bound = if !exact.is_empty() { exact } else { bare };

        match bound.len() {
            0 => {
                let id = inner
                    .default_id
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("no default agent configured"))?
                    .to_owned();
                drop(inner);
                self.get(&id)
            }
            1 => Ok(Arc::clone(&bound[0])),
            _ => {
                // Multiple matches — use the one with the lowest-alphabetical ID as tie-break.
                let winner = bound
                    .into_iter()
                    .min_by_key(|a| a.id.clone())
                    .expect("non-empty");
                Ok(winner)
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("agent registry lock poisoned")
            .agents
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .read()
            .expect("agent registry lock poisoned")
            .agents
            .is_empty()
    }

    pub fn all(&self) -> Vec<Arc<AgentHandle>> {
        self.inner
            .read()
            .expect("agent registry lock poisoned")
            .agents
            .values()
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rsclaw_config::{
        runtime::{
            AgentsRuntime, ChannelRuntime, ExtRuntime, GatewayRuntime, ModelRuntime, OpsRuntime,
            RuntimeConfig,
        },
        schema::{AgentEntry, BindMode, GatewayMode, ReloadMode, SessionConfig},
    };

    fn make_runtime(agents: Vec<AgentEntry>) -> RuntimeConfig {
        RuntimeConfig {
            gateway: GatewayRuntime {
                port: 18888,
                mode: GatewayMode::Local,
                bind: BindMode::Loopback,
                bind_address: None,
                reload: ReloadMode::Hybrid,
                auth_token: None,
                a2a_principals: vec![],
                a2a_relay: Default::default(),
                a2a_max_body_bytes: 100 * 1024 * 1024,
                allow_tailscale: false,
                channel_health_check_minutes: 5,
                channel_stale_event_threshold_minutes: 30,
                channel_max_restarts_per_hour: 10,
                auth_token_configured: false,
                auth_token_is_plaintext: false,
                user_agent: None,
                language: None,
            },
            agents: AgentsRuntime {
                defaults: Default::default(),
                list: agents,
                bindings: vec![],
                a2a: vec![],
            },
            channel: ChannelRuntime {
                channels: Default::default(),
                session: SessionConfig {
                    dm_scope: None,
                    thread_bindings: None,
                    reset: None,
                    identity_links: None,
                    maintenance: None,
                },
            },
            model: ModelRuntime {
                models: None,
                auth: None,
            },
            ext: ExtRuntime {
                tools: None,
                skills: None,
                plugins: None,
                evolution: None,
            },
            ops: OpsRuntime {
                cron: None,
                hooks: None,
                sandbox: None,
                logging: None,
                secrets: None,
            },
            raw: Default::default(),
        }
    }

    fn entry(id: &str, default: bool, channels: Option<Vec<&str>>) -> AgentEntry {
        AgentEntry {
            id: id.to_owned(),
            description: None,
            default: if default { Some(true) } else { None },
            workspace: None,
            model: None,
            flash_model: None,
            lane: None,
            lane_concurrency: None,
            group_chat: None,
            channels: channels.map(|v| v.into_iter().map(str::to_owned).collect()),
            name: None,
            commands: None,
            allowed_commands: None,
            opencode: None,
            claudecode: None,
            codex: None,
            agent_dir: None,
            system: None,
            temperature: None,
        }
    }

    #[test]
    fn routes_to_default_when_no_binding() {
        let cfg = make_runtime(vec![
            entry("main", true, None),
            entry("telegram_bot", false, Some(vec!["telegram"])),
        ]);
        let reg = AgentRegistry::from_config(&cfg);
        let agent = reg.route("slack").expect("route");
        assert_eq!(agent.id, "main");
    }

    #[test]
    fn routes_to_bound_agent() {
        let cfg = make_runtime(vec![
            entry("main", true, None),
            entry("tgbot", false, Some(vec!["telegram"])),
        ]);
        let reg = AgentRegistry::from_config(&cfg);
        let agent = reg.route("telegram").expect("route");
        assert_eq!(agent.id, "tgbot");
    }

    #[test]
    fn get_by_id() {
        let cfg = make_runtime(vec![entry("alpha", true, None)]);
        let reg = AgentRegistry::from_config(&cfg);
        assert!(reg.get("alpha").is_ok());
        assert!(reg.get("nonexistent").is_err());
    }
}
