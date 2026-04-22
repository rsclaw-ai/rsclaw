//! Dynamic agent spawning — allows new agent instances to be created at
//! runtime.

use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Result, anyhow};
use tokio::sync::{broadcast, mpsc};
use tracing::info;

use crate::{
    agent::{AgentHandle, AgentMessage, AgentRegistry, AgentReply, AgentRuntime, MemoryStore},
    config::{runtime::RuntimeConfig, schema::AgentEntry},
    events::AgentEvent,
    plugin::PluginRegistry,
    provider::registry::ProviderRegistry,
    skill::SkillRegistry,
    store::Store,
};

pub struct AgentSpawner {
    pub registry: Arc<AgentRegistry>,
    pub config: Arc<RuntimeConfig>,
    pub providers: Arc<ProviderRegistry>,
    pub skills: Arc<SkillRegistry>,
    pub store: Arc<Store>,
    pub memory: Option<Arc<tokio::sync::Mutex<MemoryStore>>>,
    pub event_tx: broadcast::Sender<AgentEvent>,
    pub plugins: Option<Arc<PluginRegistry>>,
    me: OnceLock<Weak<AgentSpawner>>,
}

impl AgentSpawner {
    /// Create an `Arc<AgentSpawner>` that holds a `Weak` self-reference for
    /// passing to child runtimes.
    #[allow(clippy::too_many_arguments)]
    pub fn new_arc(
        registry: Arc<AgentRegistry>,
        config: Arc<RuntimeConfig>,
        providers: Arc<ProviderRegistry>,
        skills: Arc<SkillRegistry>,
        store: Arc<Store>,
        memory: Option<Arc<tokio::sync::Mutex<MemoryStore>>>,
        event_tx: broadcast::Sender<AgentEvent>,
        plugins: Option<Arc<PluginRegistry>>,
    ) -> Arc<Self> {
        let s = Arc::new(Self {
            registry,
            config,
            providers,
            skills,
            store,
            memory,
            event_tx,
            plugins,
            me: OnceLock::new(),
        });
        s.me.set(Arc::downgrade(&s)).ok();
        s
    }

    /// Dynamically spawn a new agent at runtime.
    /// Returns the new agent's ID on success.
    pub fn spawn_agent(&self, entry: AgentEntry) -> Result<String> {
        self.spawn_agent_with_kind(entry, crate::agent::registry::AgentKind::Named)
    }

    /// Spawn an agent with an explicit kind.
    pub fn spawn_agent_with_kind(&self, entry: AgentEntry, kind: crate::agent::registry::AgentKind) -> Result<String> {
        let id = entry.id.clone();

        if self.registry.get(&id).is_ok() {
            return Err(anyhow!("agent '{}' already exists", id));
        }

        let (tx, mut rx) = mpsc::channel::<AgentMessage>(32);
        let max_concurrent = entry
            .lane_concurrency
            .or(self.config.agents.defaults.max_concurrent)
            .unwrap_or(4) as usize;
        let handle = Arc::new(AgentHandle {
            id: id.clone(),
            kind,
            config: entry.clone(),
            tx,
            concurrency: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
            live_status: Arc::new(tokio::sync::RwLock::new(
                crate::agent::runtime::LiveStatus::default(),
            )),
            providers: Arc::clone(&self.providers),
            abort_flags: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            started_at: std::time::Instant::now(),
            session_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_ctx_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_sys_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_tools_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_msg_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            clear_signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            new_session_signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reset_signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            memory: None,
        });

        self.registry.insert_handle(Arc::clone(&handle));

        let fallback_models = handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.fallbacks.clone())
            .unwrap_or_default();

        // Upgrade weak self-reference so child runtime can also spawn agents.
        let self_arc: Option<Arc<AgentSpawner>> = self.me.get().and_then(|w| w.upgrade());

        let mut runtime = AgentRuntime::new(
            Arc::clone(&handle),
            Arc::clone(&self.config),
            Arc::clone(&self.providers),
            fallback_models,
            Arc::clone(&self.skills),
            Arc::clone(&self.store),
            self.memory.clone(),
            Some(Arc::clone(&self.registry)),
            Some(self.event_tx.clone()),
            self_arc,
            self.plugins.clone(),
            None, // MCP registry not propagated to dynamically spawned agents
            None, // notification_tx not available for dynamically spawned agents
        );

        tokio::spawn(async move {
            info!(agent_id = %handle.id, "dynamic agent spawned");
            while let Some(msg) = rx.recv().await {
                let AgentMessage {
                    session_key,
                    text,
                    channel,
                    peer_id,
                    reply_tx,
                    extra_tools,
                    images,
                    files,
                    chat_id: _,
                } = msg;
                let result = runtime
                    .run_turn(
                        &session_key,
                        &text,
                        &channel,
                        &peer_id,
                        extra_tools,
                        images,
                        files,
                    )
                    .await;
                let reply = result.unwrap_or_else(|e| {
                    tracing::error!(agent = %handle.id, "dynamic agent turn error: {e:#}");
                    AgentReply {
                        text: format!("[error: {e}]"),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                        was_preparse: false,
                    }
                });
                let _ = reply_tx.send(reply);
            }
            info!(agent_id = %handle.id, "dynamic agent task ended");
        });

        Ok(id)
    }
}
