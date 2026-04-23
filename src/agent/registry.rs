//! Agent registry — maps agent IDs to live `AgentHandle`s.
//!
//! The registry is built at gateway startup from `RuntimeConfig.agents`
//! and is shared (via `Arc`) across all concurrent requests.
//!
//! Internal storage uses `std::sync::RwLock` so that `insert_handle` can be
//! called from outside (e.g. `AgentSpawner`) without requiring `&mut self`.

use std::{
    collections::HashMap,
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicUsize},
    time::Instant,
};

use anyhow::Result;
use tokio::sync::{RwLock, mpsc};

use crate::config::{runtime::RuntimeConfig, schema::AgentEntry};

/// Default maximum concurrent turns per agent when not configured.
const DEFAULT_MAX_CONCURRENT: u32 = 4;

// ---------------------------------------------------------------------------
// AgentHandle
// ---------------------------------------------------------------------------

/// The four kinds of agent in the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AgentKind {
    /// The default entry point. Cannot be deleted. `default: true` in config.
    Main,
    /// User-created persistent agent. Saved to config file, survives restarts.
    Named,
    /// LLM-spawned temporary agent (`persistent: false`). Lives in memory, gone on restart.
    Sub,
    /// One-shot task agent. Automatically destroyed after completion.
    Task,
}

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
    pub live_status: Arc<RwLock<crate::agent::runtime::LiveStatus>>,
    /// Provider registry for direct LLM calls (used by /btw bypass).
    pub providers: Arc<crate::provider::registry::ProviderRegistry>,
    /// Per-session abort flags: session_key -> atomic abort flag.
    /// Uses std::sync::RwLock (not tokio) so it can be accessed in Drop impls.
    pub abort_flags: Arc<std::sync::RwLock<HashMap<String, Arc<AtomicBool>>>>,
    /// When this agent handle was created (for /status uptime).
    pub started_at: Instant,
    /// Number of active sessions (updated after each turn for /status).
    pub session_count: Arc<AtomicUsize>,
    /// Per-session context token stats, updated by normal conversation LLM calls only.
    pub session_tokens: Arc<std::sync::RwLock<HashMap<String, SessionTokens>>>,
    /// Signal to clear all sessions (set by /clear bypass, consumed by runtime).
    pub clear_signal: Arc<AtomicBool>,
    /// Signal to start a new session (set by /new bypass, consumed by runtime).
    /// Unlike clear_signal, this increments the archive generation.
    pub new_session_signal: Arc<AtomicBool>,
    /// Signal to reset current session (set by /reset bypass, consumed by runtime).
    /// Clears session without summary or generation increment.
    pub reset_signal: Arc<AtomicBool>,
    /// Shared memory store for this agent (used by meditation heartbeat).
    pub memory: Option<Arc<tokio::sync::Mutex<crate::agent::memory::MemoryStore>>>,
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

    /// Unified status string used by all channels (desktop WS, feishu, telegram, etc.).
    pub fn format_status(&self) -> String {
        use crate::agent::prompt_builder::format_duration;

        let model = self.config.model.as_ref()
            .and_then(|m| m.primary.as_deref())
            .unwrap_or("default");
        let sessions = self.session_count.load(std::sync::atomic::Ordering::Relaxed);
        let uptime = format_duration(self.started_at.elapsed());
        let os = if cfg!(target_os = "macos") { "macOS" }
            else if cfg!(target_os = "linux") {
                if std::env::var("ANDROID_ROOT").is_ok() { "Android" } else { "Linux" }
            }
            else if cfg!(target_os = "windows") { "Windows" }
            else if cfg!(target_os = "ios") { "iOS" }
            else { "Unknown" };
        let ctx_limit = self.config.model.as_ref()
            .and_then(|m| m.context_tokens)
            .unwrap_or(64000) as usize;

        let mut ctx_lines = String::new();
        if let Ok(map) = self.session_tokens.read() {
            if map.is_empty() {
                ctx_lines.push_str("Context: (no sessions)\n");
            } else {
                for (key, t) in map.iter() {
                    let short_key = if key.len() > 20 { &key[..20] } else { key };
                    let pct = if ctx_limit > 0 { t.total * 100 / ctx_limit } else { 0 };
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

        format!(
            "Gateway: running\nOS: {os}\nModel: {model}\nSessions: {sessions}\n\
             {ctx_lines}\
             Uptime: {uptime}\nVersion: rsclaw {}",
            option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
        )
    }
}

/// An image attachment sent by the user.
#[derive(Debug, Clone)]
pub struct ImageAttachment {
    /// Base64-encoded data URI or URL.
    pub data: String,
    /// MIME type (e.g. "image/png", "image/jpeg").
    pub mime_type: String,
}

/// A file attachment sent by the user (raw bytes, not yet processed).
#[derive(Debug)]
pub struct FileAttachment {
    pub filename: String,
    pub data: Vec<u8>,
    pub mime_type: String,
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
    /// Peer / user ID (used for session isolation).
    pub peer_id: String,
    /// Chat/conversation ID for replies (for platforms like Feishu where reply ID differs from user ID).
    /// If empty, defaults to peer_id.
    pub chat_id: String,
    /// One-shot sender for the agent's response.
    pub reply_tx: tokio::sync::oneshot::Sender<AgentReply>,
    /// External tool definitions forwarded from the OAI /v1/chat/completions
    /// caller. These are merged into the agent's tool list for the turn.
    pub extra_tools: Vec<crate::provider::ToolDef>,
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
            let mime = if lower.ends_with(".png") {
                "image/png"
            } else if lower.ends_with(".gif") {
                "image/gif"
            } else if lower.ends_with(".webp") {
                "image/webp"
            } else {
                "image/jpeg"
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            images.push(ImageAttachment {
                data: format!("data:{mime};base64,{b64}"),
                mime_type: mime.to_owned(),
            });
        } else {
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path_str.to_owned());
            let mime = match lower.rsplit('.').next() {
                Some("pdf") => "application/pdf",
                Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                Some("txt" | "md" | "rs" | "py" | "js" | "ts" | "json" | "toml" | "yaml" | "yml") => "text/plain",
                Some("mp4") => "video/mp4",
                Some("mp3") => "audio/mpeg",
                Some("wav") => "audio/wav",
                _ => "application/octet-stream",
            }.to_owned();
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
    /// True when the reply was produced by the preparse engine (not LLM).
    /// The outer agent loop uses this to decide whether to emit to event_bus,
    /// since agent_loop already emits for LLM turns.
    pub was_preparse: bool,
}

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
        let providers = Arc::new(crate::provider::registry::ProviderRegistry::new());
        let (registry, _) = Self::from_config_with_receivers(cfg, providers);
        registry
    }

    /// Build a registry AND return the mpsc receivers for each agent so the
    /// gateway can spawn real runtime tasks.
    pub fn from_config_with_receivers(
        cfg: &RuntimeConfig,
        providers: Arc<crate::provider::registry::ProviderRegistry>,
    ) -> (Self, HashMap<String, mpsc::Receiver<AgentMessage>>) {
        let registry = Self::new();
        let mut receivers = HashMap::new();

        // If agents.list is empty (e.g. openclaw.json with no explicit list),
        // synthesize a default agent from agents.defaults.
        let agent_list = if cfg.agents.list.is_empty() {
            let model = cfg.agents.defaults.model.clone();
            let workspace = cfg.agents.defaults.workspace.clone();
            vec![crate::config::schema::AgentEntry {
                id: "main".to_owned(),
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
                opencode: None,
                claudecode: None,
                agent_dir: None,
                system: None,
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
            let mut inner = registry.inner.write().expect("agent registry lock poisoned");

            for entry in &agent_list {
                let (tx, rx) = mpsc::channel::<AgentMessage>(32);
                let permits = entry
                    .lane_concurrency
                    .map(|n| n as usize)
                    .unwrap_or(max_concurrent);
                let kind = if entry.default.unwrap_or(false) {
                    AgentKind::Main
                } else {
                    AgentKind::Named
                };
                let handle = Arc::new(AgentHandle {
                    id: entry.id.clone(),
                    kind,
                    config: entry.clone(),
                    tx,
                    concurrency: Arc::new(tokio::sync::Semaphore::new(permits)),
                    live_status: Arc::new(
                        RwLock::new(crate::agent::runtime::LiveStatus::default()),
                    ),
                    providers: Arc::clone(&providers),
                    abort_flags: Arc::new(std::sync::RwLock::new(HashMap::new())),
                    started_at: Instant::now(),
                    session_count: Arc::new(AtomicUsize::new(0)),
                    session_tokens: Arc::new(std::sync::RwLock::new(HashMap::new())),
                    clear_signal: Arc::new(AtomicBool::new(false)),
                    new_session_signal: Arc::new(AtomicBool::new(false)),
                    reset_signal: Arc::new(AtomicBool::new(false)),
                    memory: None, // populated by agent runtime after memory store opens
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
    ///   1. Agents with `channels` containing `"channel:account"` (exact match).
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
        self.inner.read().expect("agent registry lock poisoned").agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().expect("agent registry lock poisoned").agents.is_empty()
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
    use crate::config::{
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
                external: vec![],
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
            agent_dir: None,
            system: None,
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
