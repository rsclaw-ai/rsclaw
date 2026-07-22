//! Dynamic agent spawning — allows new agent instances to be created at
//! runtime.

use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Result, anyhow};
use rsclaw_config::{live_config::LiveConfig, runtime::RuntimeConfig, schema::AgentEntry};
use rsclaw_events::AgentEvent;
use rsclaw_plugin::PluginRegistry;
use rsclaw_provider::registry::ProviderRegistry;
use rsclaw_skill::SkillRegistry;
use rsclaw_store::Store;
use tokio::sync::{broadcast, mpsc};
use tracing::info;

use crate::{
    AgentHandle, AgentKind, AgentMessage, AgentRegistry, AgentReply, AgentRuntime, MemoryStore,
};

pub struct AgentSpawner {
    pub registry: Arc<AgentRegistry>,
    pub config: Arc<RuntimeConfig>,
    /// Live, hot-mutable config slices (temperature, etc.) shared across all
    /// runtimes spawned by this spawner.
    pub live: Arc<LiveConfig>,
    /// Shared provider slot — swapped by `rsclaw reload --scope providers`.
    /// Dynamically spawned agents clone this slot into their handle, so they
    /// always see the latest registry without needing a per-handle set_providers.
    pub providers: Arc<std::sync::RwLock<Arc<ProviderRegistry>>>,
    pub skills: Arc<SkillRegistry>,
    pub store: Arc<Store>,
    pub memory: Option<Arc<tokio::sync::Mutex<MemoryStore>>>,
    pub event_tx: broadcast::Sender<AgentEvent>,
    pub plugins: Option<Arc<PluginRegistry>>,
    /// Per-model health table — same `Arc` is held by gateway state &
    /// every spawned runtime's FailoverManager. Sharing means dynamically
    /// spawned sub-agents see the same Disabled/Cooling decisions as the
    /// main loop, so a balance-out doubao trips the chain once globally.
    pub model_health: rsclaw_provider::health::ProviderHealthRegistry,
    /// Coding-agent cap manager — shared from gateway startup so dynamically
    /// spawned agents also have `tool_cap` available.
    pub cap_manager: Option<std::sync::Arc<rsclaw_cap::CapAgentManager>>,
    /// Interactive multi-instance cap session manager, shared identically.
    pub cap_live_manager: Option<std::sync::Arc<rsclaw_cap::CapLiveManager>>,
    me: OnceLock<Weak<AgentSpawner>>,
}

impl AgentSpawner {
    /// Create an `Arc<AgentSpawner>` that holds a `Weak` self-reference for
    /// passing to child runtimes.
    #[allow(clippy::too_many_arguments)]
    pub fn new_arc(
        registry: Arc<AgentRegistry>,
        config: Arc<RuntimeConfig>,
        live: Arc<LiveConfig>,
        providers: Arc<std::sync::RwLock<Arc<ProviderRegistry>>>,
        skills: Arc<SkillRegistry>,
        store: Arc<Store>,
        memory: Option<Arc<tokio::sync::Mutex<MemoryStore>>>,
        event_tx: broadcast::Sender<AgentEvent>,
        plugins: Option<Arc<PluginRegistry>>,
        model_health: rsclaw_provider::health::ProviderHealthRegistry,
        cap_manager: Option<std::sync::Arc<rsclaw_cap::CapAgentManager>>,
        cap_live_manager: Option<std::sync::Arc<rsclaw_cap::CapLiveManager>>,
    ) -> Arc<Self> {
        let s = Arc::new(Self {
            registry,
            config,
            live,
            providers,
            skills,
            store,
            memory,
            event_tx,
            plugins,
            model_health,
            cap_manager,
            cap_live_manager,
            me: OnceLock::new(),
        });
        s.me.set(Arc::downgrade(&s)).ok();
        s
    }

    /// Dynamically spawn a new agent at runtime.
    /// Returns the new agent's ID on success.
    pub fn spawn_agent(&self, entry: AgentEntry) -> Result<String> {
        self.spawn_agent_with_kind(entry, AgentKind::Named)
    }

    /// Dynamically spawn a new agent at runtime with explicit kind.
    /// Returns the new agent's ID on success.
    pub fn spawn_agent_with_kind(&self, entry: AgentEntry, kind: AgentKind) -> Result<String> {
        let id = entry.id.clone();

        if self.registry.get(&id).is_ok() {
            return Err(anyhow!("agent '{}' already exists", id));
        }

        let (tx, mut rx) = mpsc::channel::<AgentMessage>(32);
        let max_concurrent = entry
            .lane_concurrency
            .or(self.config.agents.defaults.max_concurrent)
            .unwrap_or(4) as usize;
        let context_window = entry
            .model
            .as_ref()
            .and_then(|m| m.context_tokens)
            .or(self.config.agents.defaults.context_tokens)
            .unwrap_or(0) as usize;
        let effective_model =
            crate::runtime::resolve_primary_model_for(&entry, &self.config.agents.defaults)
                .unwrap_or_else(|| "rsclaw/rsclaw-agent-v1".to_owned());
        let handle = Arc::new(AgentHandle {
            id: id.clone(),
            kind,
            config: entry.clone(),
            tx,
            concurrency: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
            live_status: Arc::new(tokio::sync::RwLock::new(
                crate::runtime::LiveStatus::default(),
            )),
            providers: Arc::clone(&self.providers),
            abort_flags: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            cancel_tokens: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            lifetime: tokio_util::sync::CancellationToken::new(),
            plugin_overrides: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            wasm_plugins: Arc::new(std::sync::RwLock::new(Arc::new(Vec::new()))),
            notification_tx: Arc::new(std::sync::RwLock::new(None)),
            cold_enabled: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            started_at: std::time::Instant::now(),
            session_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            session_tokens: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            last_ctx_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_sys_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_tools_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_msg_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            clear_signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            new_session_signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            context_window,
            effective_model,
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

        let providers_snapshot = self
            .providers
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|_| Arc::new(ProviderRegistry::new()));

        let mut runtime = AgentRuntime::new(
            Arc::clone(&handle),
            Arc::clone(&self.config),
            Arc::clone(&self.live),
            providers_snapshot,
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
            self.model_health.clone(),
            self.cap_manager.clone(),
            self.cap_live_manager.clone(),
        );

        // Capture for i18n lookup on the error reply path inside the
        // spawned task.
        let config_for_task = Arc::clone(&self.config);
        tokio::spawn(async move {
            info!(agent_id = %handle.id, "dynamic agent spawned");
            loop {
                let msg = tokio::select! {
                    _ = handle.lifetime.cancelled() => {
                        info!(agent_id = %handle.id, "dynamic agent lifetime cancelled, stopping");
                        break;
                    }
                    msg = rx.recv() => match msg {
                        Some(m) => m,
                        None => break,
                    },
                };
                let AgentMessage {
                    session_key,
                    text,
                    channel,
                    account,
                    peer_id,
                    reply_tx,
                    extra_tools,
                    images,
                    files,
                    chat_id,
                    ..
                } = msg;
                let result = tokio::select! {
                    r = runtime.run_turn(
                        &session_key,
                        &text,
                        &channel,
                        &peer_id,
                        &chat_id,
                        account.as_deref(),
                        extra_tools,
                        images,
                        files,
                        crate::registry::TurnContext::default(),
                    ) => r,
                    _ = handle.lifetime.cancelled() => {
                        info!(agent_id = %handle.id, "dynamic agent lifetime cancelled mid-turn");
                        break;
                    }
                };
                let reply = result.unwrap_or_else(|e| {
                    tracing::error!(agent = %handle.id, "dynamic agent turn error: {e:#}");
                    let outcome = if e.to_string().contains("canceled by A2A CancelTask") {
                        crate::registry::ReplyOutcome::Canceled
                    } else {
                        crate::registry::ReplyOutcome::Error
                    };
                    // i18n the user-facing text. Raw anyhow Error stays
                    // in the ERROR log above; the chat channel sees a
                    // friendly localized message. Mirrors the same
                    // pattern in gateway::startup::spawn_agent_tasks.
                    let i18n_lang = config_for_task
                        .raw
                        .gateway
                        .as_ref()
                        .and_then(|g| g.language.as_deref())
                        .map(rsclaw_i18n::resolve_lang)
                        .unwrap_or("en");
                    let user_text = match outcome {
                        crate::registry::ReplyOutcome::Canceled => "[canceled]".to_owned(),
                        _ => rsclaw_i18n::t("backend_unavailable", i18n_lang),
                    };
                    AgentReply {
                        text: user_text,
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                        needs_outer_done_emit: false,
                        outcome,
                    }
                });
                let _ = reply_tx.send(reply);
            }
            info!(agent_id = %handle.id, "dynamic agent task ended");
        });

        Ok(id)
    }
}
