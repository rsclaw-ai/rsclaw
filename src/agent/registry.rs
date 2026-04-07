//! Agent registry — maps agent IDs to live `AgentHandle`s.
//!
//! The registry is built at gateway startup from `RuntimeConfig.agents`
//! and is shared (via `Arc`) across all concurrent requests.
//!
//! Internal storage uses `std::sync::RwLock` so that `insert_handle` can be
//! called from outside (e.g. `AgentSpawner`) without requiring `&mut self`.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use tokio::sync::{RwLock, mpsc};

use crate::config::{runtime::RuntimeConfig, schema::AgentEntry};

/// Default maximum concurrent turns per agent when not configured.
const DEFAULT_MAX_CONCURRENT: u32 = 4;

// ---------------------------------------------------------------------------
// AgentHandle
// ---------------------------------------------------------------------------

/// A lightweight handle to a running agent.
///
/// The actual agent loop lives in a tokio task; communication happens
/// through channels. This struct is cheaply `Clone`-able.
#[derive(Clone)]
pub struct AgentHandle {
    pub id: String,
    pub config: AgentEntry,
    /// Sender to the agent's message queue.
    pub tx: mpsc::Sender<AgentMessage>,
    /// Limits concurrent turns for this agent.
    pub concurrency: Arc<tokio::sync::Semaphore>,
    /// Shared live status for /btw parallel queries.
    pub live_status: Arc<RwLock<crate::agent::runtime::LiveStatus>>,
    /// Provider registry for direct LLM calls (used by /btw bypass).
    pub providers: Arc<crate::provider::registry::ProviderRegistry>,
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
    /// If set, the worker should send a follow-up LLM analysis after the
    /// immediate reply.
    pub pending_analysis: Option<PendingAnalysis>,
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
            let mut inner = registry.inner.write().unwrap();

            for entry in &agent_list {
                let (tx, rx) = mpsc::channel::<AgentMessage>(32);
                let permits = entry
                    .lane_concurrency
                    .map(|n| n as usize)
                    .unwrap_or(max_concurrent);
                let handle = Arc::new(AgentHandle {
                    id: entry.id.clone(),
                    config: entry.clone(),
                    tx,
                    concurrency: Arc::new(tokio::sync::Semaphore::new(permits)),
                    live_status: Arc::new(
                        RwLock::new(crate::agent::runtime::LiveStatus::default()),
                    ),
                    providers: Arc::clone(&providers),
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
        let mut inner = self.inner.write().unwrap();
        inner.agents.insert(handle.id.clone(), handle);
    }

    /// Look up an agent by ID.
    pub fn get(&self, id: &str) -> Result<Arc<AgentHandle>> {
        self.inner
            .read()
            .unwrap()
            .agents
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("agent not found: `{id}`"))
    }

    /// Return the default agent.
    pub fn default_agent(&self) -> Result<Arc<AgentHandle>> {
        let inner = self.inner.read().unwrap();
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
        let inner = self.inner.read().unwrap();

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
        self.inner.read().unwrap().agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().agents.is_empty()
    }

    pub fn all(&self) -> Vec<Arc<AgentHandle>> {
        self.inner
            .read()
            .unwrap()
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
