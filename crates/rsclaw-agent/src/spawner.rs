//! Dynamic agent spawning — allows new agent instances to be created at
//! runtime.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, Weak, atomic::AtomicBool},
};

use anyhow::{Result, anyhow};
use rsclaw_config::{live_config::LiveConfig, runtime::RuntimeConfig, schema::AgentEntry};
use rsclaw_events::AgentEvent;
use rsclaw_plugin::PluginRegistry;
use rsclaw_provider::registry::ProviderRegistry;
use rsclaw_skill::SkillRegistry;
use rsclaw_store::Store;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use crate::{
    AgentHandle, AgentKind, AgentMessage, AgentRegistry, AgentReply, AgentRuntime, MemoryStore,
};

/// Process one queued [`AgentMessage`] on `runtime`: the shared body of every
/// agent worker loop (boot-time agents in gateway startup, dynamically
/// spawned and hot-replaced agents here).
///
/// Carries the sender trust and A2A wires into the [`crate::registry::TurnContext`],
/// mints and registers a per-turn hard-cancel token (so `chat.abort` works),
/// runs the stuck-turn watchdog, maps errors to an i18n reply with the right
/// [`crate::registry::ReplyOutcome`], emits the terminal `done` event when
/// `agent_loop` did not, applies the `/goal` post-turn hook and finally sends
/// the reply on `reply_tx`.
pub async fn process_queued_message(
    runtime: &mut AgentRuntime,
    msg: AgentMessage,
    goal_continuation: Option<&GoalContinuationFn>,
) {
    let handle = Arc::clone(&runtime.handle);
    let AgentMessage {
        session_key,
        text,
        channel,
        peer_id,
        chat_id,
        reply_tx,
        extra_tools,
        images,
        files,
        account,
        task_id,
        context_id,
        cancel_token,
        event_tx,
        input_request_tx,
        trust,
    } = msg;
    // Hard-cancel token for this turn. A2A callers supply their own
    // (CancelTask). For everyone else (WS chat.abort, channels) we mint one
    // and register it under the session key so chat.abort can fire
    // `.cancel()` — the `tokio::select!` below then drops the in-flight
    // `run_turn` future immediately, even if it's parked on a stalled LLM
    // stream. `registered` tracks whether we own the map entry.
    let (turn_token, registered) = match cancel_token {
        Some(t) => (t, false),
        None => {
            let t = tokio_util::sync::CancellationToken::new();
            match handle.cancel_tokens.write() {
                Ok(mut toks) => {
                    toks.insert(session_key.clone(), t.clone());
                }
                Err(e) => warn!(agent = %handle.id, "cancel_tokens lock poisoned: {e}"),
            }
            (t, true)
        }
    };

    let is_daemon = runtime.is_daemon_agent(&handle.id);
    // Progress heartbeat for the daemon watchdog (bumped once per agent-loop
    // iteration via TurnContext::progress_tick).
    let progress = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let turn_ctx = crate::registry::TurnContext {
        task_id,
        context_id,
        event_tx,
        cancel_token: Some(turn_token.clone()),
        input_request_tx,
        progress: Some(Arc::clone(&progress)),
        trust,
    };
    let watchdog = spawn_turn_watchdog(
        turn_token.clone(),
        progress,
        is_daemon,
        handle.id.clone(),
        session_key.clone(),
    );
    let result = tokio::select! {
        biased;
        // Hard cancel: drops the run_turn future (and every await it holds)
        // the moment the token fires.
        _ = turn_token.cancelled() => Err(anyhow!("turn aborted")),
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
            turn_ctx,
        ) => r,
    };
    watchdog.abort();
    // Drop our registered token so a later abort for this session can't
    // cancel a future turn, and the map doesn't leak.
    if registered {
        match handle.cancel_tokens.write() {
            Ok(mut toks) => {
                toks.remove(&session_key);
            }
            Err(e) => warn!(agent = %handle.id, "cancel_tokens lock poisoned: {e}"),
        }
    }

    let turn_errored = result.is_err();
    let reply = result.unwrap_or_else(|e| {
        // A2A consumers key off `outcome` to publish the right terminal
        // status (Failed vs Canceled); "turn aborted" is the WS abort path,
        // "canceled by A2A CancelTask" the A2A one.
        let err_str = e.to_string();
        let is_user_cancel =
            err_str.contains("canceled by A2A CancelTask") || err_str.contains("turn aborted");
        if is_user_cancel {
            info!(agent = %handle.id, "turn canceled by user: {e:#}");
        } else {
            error!(agent = %handle.id, "turn error: {e:#}");
        }
        let outcome = if is_user_cancel {
            crate::registry::ReplyOutcome::Canceled
        } else {
            crate::registry::ReplyOutcome::Error
        };
        // Never leak the raw error chain to the chat channel — it stays in
        // the log above; the user sees a localized line.
        let i18n_lang = runtime
            .config
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

    // Emit to the event bus for any reply path that bypassed agent_loop
    // (preparse, file-attach short-circuits, /btw, …) and for turns that
    // failed with Err — agent_loop never emitted `done` for those, and WS
    // clients would otherwise wait for the terminator forever.
    if (reply.needs_outer_done_emit || turn_errored)
        && let Some(bus) = runtime.event_bus.as_ref()
    {
        if !reply.text.is_empty()
            && bus
                .send(AgentEvent {
                    session_id: session_key.clone(),
                    agent_id: handle.id.clone(),
                    delta: reply.text.clone(),
                    done: false,
                    files: vec![],
                    images: vec![],
                    tool_log: vec![],
                    question: None,
                    channel: None,
                })
                .is_err()
        {
            tracing::debug!(agent = %handle.id, "event bus has no subscribers (reply delta)");
        }
        if bus
            .send(AgentEvent {
                session_id: session_key.clone(),
                agent_id: handle.id.clone(),
                delta: String::new(),
                done: true,
                files: vec![],
                images: vec![],
                tool_log: vec![],
                question: None,
                channel: None,
            })
            .is_err()
        {
            tracing::debug!(agent = %handle.id, "event bus has no subscribers (done)");
        }
    }

    // /goal — completion-driven turn loop (see `crate::goal`). After every
    // successful turn with an active goal we either append the terminal
    // status to this reply, or enqueue the next iteration.
    let mut reply = reply;
    if !turn_errored
        && let Some(reaction) = crate::goal::check_after_turn(&session_key, &reply.text).await
    {
        match reaction {
            crate::goal::Reaction::Done(status_line) => {
                // Replace the machine GOAL_ marker line with the
                // human-friendly status line.
                reply.text = crate::goal::strip_trailing_goal_marker(&reply.text);
                if !reply.text.is_empty() {
                    reply.text.push_str("\n\n");
                }
                reply.text.push_str(&status_line);
                reply.is_empty =
                    reply.text.is_empty() && reply.images.is_empty() && reply.files.is_empty();
            }
            crate::goal::Reaction::Continue(next_prompt) => match goal_continuation {
                Some(enqueue) => {
                    let delivery_channel: &str =
                        if channel == "ws" { "desktop" } else { &channel };
                    enqueue(&session_key, &next_prompt, delivery_channel, &peer_id);
                }
                None => warn!(
                    session = %session_key,
                    "/goal: no continuation hook installed; cannot enqueue next iteration"
                ),
            },
        }
    }
    // The receiver may already be gone (e.g. a channel-side timeout).
    if reply_tx.send(reply).is_err() {
        tracing::debug!(agent = %handle.id, session = %session_key, "reply receiver dropped");
    }
}

/// Stuck-turn watchdog. Normal turns: flat 20-minute wall-clock cap. Daemon
/// agents loop forever, so instead watch the progress counter: if it stops
/// advancing for 3 minutes a tool is wedged, so cancel (cron restarts it).
fn spawn_turn_watchdog(
    token: tokio_util::sync::CancellationToken,
    progress: Arc<std::sync::atomic::AtomicU64>,
    is_daemon: bool,
    agent_id: String,
    session_key: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        if is_daemon {
            const POLL: std::time::Duration = std::time::Duration::from_secs(30);
            const STALL_LIMIT: std::time::Duration = std::time::Duration::from_secs(180);
            let mut last = progress.load(Ordering::Relaxed);
            let mut stalled = std::time::Duration::ZERO;
            loop {
                tokio::time::sleep(POLL).await;
                if token.is_cancelled() {
                    break;
                }
                let cur = progress.load(Ordering::Relaxed);
                if cur != last {
                    last = cur;
                    stalled = std::time::Duration::ZERO;
                    continue;
                }
                stalled += POLL;
                if stalled >= STALL_LIMIT {
                    error!(
                        agent = %agent_id,
                        session = %session_key,
                        stall_s = stalled.as_secs(),
                        "stuck-turn watchdog (daemon): no loop progress — a tool is \
                         wedged; firing cancel_token (cron will restart)"
                    );
                    token.cancel();
                    break;
                }
            }
        } else {
            const TURN_WALL_CLOCK_LIMIT: std::time::Duration =
                std::time::Duration::from_secs(20 * 60);
            tokio::time::sleep(TURN_WALL_CLOCK_LIMIT).await;
            if !token.is_cancelled() {
                error!(
                    agent = %agent_id,
                    session = %session_key,
                    limit_s = TURN_WALL_CLOCK_LIMIT.as_secs(),
                    "stuck-turn watchdog: firing cancel_token — turn exceeded \
                     wall-clock limit; the agent queue must not stay dark"
                );
                token.cancel();
            }
        }
    })
}

pub struct AgentSpawner {
    pub registry: Arc<AgentRegistry>,
    /// Shared config slot — swapped by `rsclaw reload` so dynamically spawned
    /// agents pick up fresh defaults (model.vision, context_tokens, etc.).
    pub config: Arc<std::sync::RwLock<Arc<RuntimeConfig>>>,
    /// Live, hot-mutable config slices (temperature, etc.) shared across all
    /// runtimes spawned by this spawner.
    pub live: Arc<LiveConfig>,
    /// Shared provider slot — swapped by `rsclaw reload --scope providers`.
    /// Dynamically spawned agents clone this slot into their handle, so they
    /// always see the latest registry without needing a per-handle
    /// set_providers.
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
    /// Gateway-level wiring (MCP, notifications, computer-use, `/goal`
    /// continuation) applied to every runtime this spawner creates. Set once
    /// by gateway startup via [`AgentSpawner::set_runtime_wiring`].
    wiring: std::sync::RwLock<RuntimeWiring>,
    me: OnceLock<Weak<AgentSpawner>>,
}

/// Callback that enqueues the next `/goal` iteration for a session.
/// Arguments: `(session_key, next_prompt, channel, peer_id)`. Installed by the
/// gateway, which owns the task queue.
pub type GoalContinuationFn = Arc<dyn Fn(&str, &str, &str, &str) + Send + Sync>;

/// Gateway-level plumbing that `AgentRuntime::new` does not take but every
/// agent worker needs. Boot-time agents get it directly from gateway startup;
/// dynamically spawned and hot-replaced agents get the same values through
/// [`AgentSpawner::set_runtime_wiring`], so a config reload no longer strips
/// MCP tools, notifications, computer-use or `/goal` continuation.
#[derive(Clone, Default)]
pub struct RuntimeWiring {
    pub mcp: Option<Arc<rsclaw_mcp::McpRegistry>>,
    pub notification_tx: Option<broadcast::Sender<rsclaw_channel::OutboundMessage>>,
    pub computer_permission: Option<Arc<rsclaw_computer::permission::RedbPermissionStore>>,
    pub computer_permission_tx:
        Option<broadcast::Sender<rsclaw_computer::permission::PermissionRequest>>,
    pub computer_status_tx: Option<broadcast::Sender<rsclaw_computer::status::ComputerUseStatus>>,
    pub computer_runs: Option<Arc<tokio::sync::RwLock<HashMap<String, Arc<AtomicBool>>>>>,
    pub goal_continuation: Option<GoalContinuationFn>,
}

impl AgentSpawner {
    /// Create an `Arc<AgentSpawner>` that holds a `Weak` self-reference for
    /// passing to child runtimes.
    #[allow(clippy::too_many_arguments)]
    pub fn new_arc(
        registry: Arc<AgentRegistry>,
        config: Arc<std::sync::RwLock<Arc<RuntimeConfig>>>,
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
            wiring: std::sync::RwLock::new(RuntimeWiring::default()),
            me: OnceLock::new(),
        });
        s.me.set(Arc::downgrade(&s)).ok();
        s
    }

    /// Install the gateway-level wiring applied to every runtime this spawner
    /// creates from now on (see [`RuntimeWiring`]).
    pub fn set_runtime_wiring(&self, wiring: RuntimeWiring) {
        match self.wiring.write() {
            Ok(mut g) => *g = wiring,
            Err(e) => warn!("spawner wiring lock poisoned: {e}"),
        }
    }

    fn wiring_snapshot(&self) -> RuntimeWiring {
        self.wiring.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Dynamically spawn a new agent at runtime.
    /// Returns the new agent's ID on success.
    pub fn spawn_agent(&self, entry: AgentEntry) -> Result<String> {
        self.spawn_agent_with_kind(entry, AgentKind::Named)
    }

    /// Replace an existing agent atomically — used during hot-reload to swap
    /// agent handles without a gap where the agent is missing from the
    /// registry. The old handle's lifetime token is cancelled as a side
    /// effect. Hot-swappable slots (WASM/JS plugins, skills, notification
    /// sender, per-session plugin overrides) are shared with the old handle so
    /// a reload doesn't make them vanish.
    pub fn replace_agent(&self, entry: AgentEntry) -> Result<String> {
        let id = entry.id.clone();
        let old = self.registry.get(&id).ok();
        let (handle, rx, config) = self.build_handle(entry, AgentKind::Named, old.as_deref())?;
        // Atomic swap: old handle is removed and its lifetime cancelled inside
        // replace_handle.
        self.registry.replace_handle(Arc::clone(&handle));
        self.start_worker(handle, rx, config, "dynamic agent replaced (hot-reload)");
        Ok(id)
    }

    /// Dynamically spawn a new agent at runtime with explicit kind.
    /// Returns the new agent's ID on success.
    pub fn spawn_agent_with_kind(&self, entry: AgentEntry, kind: AgentKind) -> Result<String> {
        let id = entry.id.clone();

        if self.registry.get(&id).is_ok() {
            return Err(anyhow!("agent '{}' already exists", id));
        }

        let (handle, rx, config) = self.build_handle(entry, kind, None)?;
        self.registry.insert_handle(Arc::clone(&handle));
        self.start_worker(handle, rx, config, "dynamic agent spawned");
        Ok(id)
    }

    /// Build a fresh handle for `entry`. With `inherit`, hot-swappable slots
    /// are shared with that (outgoing) handle; otherwise they start from the
    /// current values of the default agent.
    fn build_handle(
        &self,
        entry: AgentEntry,
        kind: AgentKind,
        inherit: Option<&AgentHandle>,
    ) -> Result<(
        Arc<AgentHandle>,
        mpsc::Receiver<AgentMessage>,
        Arc<RuntimeConfig>,
    )> {
        let config = self
            .config
            .read()
            .map(|g| Arc::clone(&g))
            .map_err(|_| anyhow!("config rwlock poisoned"))?;

        let (tx, rx) = mpsc::channel::<AgentMessage>(32);
        // 0 would create a semaphore no turn can ever acquire.
        let max_concurrent = entry
            .lane_concurrency
            .or(config.agents.defaults.max_concurrent)
            .unwrap_or(4)
            .max(1) as usize;
        let context_window = entry
            .model
            .as_ref()
            .and_then(|m| m.context_tokens)
            .or(config.agents.defaults.context_tokens)
            .unwrap_or(0) as usize;
        let effective_model =
            crate::runtime::resolve_primary_model_for(&entry, &config.agents.defaults)
                .unwrap_or_else(|| "rsclaw/rsclaw-agent-v1".to_owned());

        let wiring = self.wiring_snapshot();
        let template = self.registry.default_agent().ok();
        let (wasm_plugins, notification_tx, skills, js_plugins, plugin_overrides, cold_enabled) =
            match inherit {
                Some(old) => (
                    Arc::clone(&old.wasm_plugins),
                    Arc::clone(&old.notification_tx),
                    Arc::clone(&old.skills),
                    Arc::clone(&old.js_plugins),
                    Arc::clone(&old.plugin_overrides),
                    Arc::clone(&old.cold_enabled),
                ),
                None => (
                    Arc::new(std::sync::RwLock::new(
                        template
                            .as_ref()
                            .map(|t| t.wasm_plugins_snapshot())
                            .unwrap_or_else(|| Arc::new(Vec::new())),
                    )),
                    Arc::new(std::sync::RwLock::new(
                        template
                            .as_ref()
                            .and_then(|t| t.notification_tx())
                            .or_else(|| wiring.notification_tx.clone()),
                    )),
                    Arc::new(std::sync::RwLock::new(
                        template
                            .as_ref()
                            .map(|t| t.skills_snapshot())
                            .unwrap_or_else(|| Arc::clone(&self.skills)),
                    )),
                    Arc::new(std::sync::RwLock::new(
                        template
                            .as_ref()
                            .and_then(|t| t.js_plugins_snapshot())
                            .or_else(|| self.plugins.clone()),
                    )),
                    Arc::new(std::sync::RwLock::new(HashMap::new())),
                    Arc::new(std::sync::RwLock::new(HashMap::new())),
                ),
            };

        let handle = Arc::new(AgentHandle {
            id: entry.id.clone(),
            kind,
            config: entry,
            tx,
            concurrency: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
            live_status: Arc::new(tokio::sync::RwLock::new(
                crate::runtime::LiveStatus::default(),
            )),
            providers: Arc::clone(&self.providers),
            abort_flags: Arc::new(std::sync::RwLock::new(HashMap::new())),
            cancel_tokens: Arc::new(std::sync::RwLock::new(HashMap::new())),
            lifetime: tokio_util::sync::CancellationToken::new(),
            plugin_overrides,
            wasm_plugins,
            notification_tx,
            cold_enabled,
            started_at: std::time::Instant::now(),
            session_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            session_tokens: Arc::new(std::sync::RwLock::new(HashMap::new())),
            last_ctx_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_sys_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_tools_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_msg_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            session_resets: Arc::new(std::sync::Mutex::new(Vec::new())),
            context_window,
            effective_model,
            skills,
            js_plugins,
        });
        Ok((handle, rx, config))
    }

    /// Build the runtime for `handle` (with the gateway wiring applied) and
    /// spawn its worker loop, which runs [`process_queued_message`] per
    /// message until the handle's lifetime token fires.
    fn start_worker(
        &self,
        handle: Arc<AgentHandle>,
        mut rx: mpsc::Receiver<AgentMessage>,
        config: Arc<RuntimeConfig>,
        started_msg: &'static str,
    ) {
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

        let wiring = self.wiring_snapshot();
        let mut runtime = AgentRuntime::new(
            Arc::clone(&handle),
            config,
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
            wiring.mcp.clone(),
            handle.notification_tx().or_else(|| wiring.notification_tx.clone()),
            self.model_health.clone(),
            self.cap_manager.clone(),
            self.cap_live_manager.clone(),
        );
        // run_turn refreshes plugins/skills from the handle each turn; seed
        // the first turn from the handle too.
        runtime.wasm_plugins = handle.wasm_plugins_snapshot();
        runtime.computer_permission = wiring.computer_permission.clone();
        runtime.computer_permission_tx = wiring.computer_permission_tx.clone();
        runtime.computer_status_tx = wiring.computer_status_tx.clone();
        runtime.computer_runs = wiring.computer_runs.clone();
        let goal_continuation = wiring.goal_continuation.clone();

        tokio::spawn(async move {
            info!(agent_id = %handle.id, "{started_msg}");
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
                tokio::select! {
                    _ = process_queued_message(&mut runtime, msg, goal_continuation.as_ref()) => {}
                    _ = handle.lifetime.cancelled() => {
                        info!(agent_id = %handle.id, "dynamic agent lifetime cancelled mid-turn");
                        break;
                    }
                }
            }
            info!(agent_id = %handle.id, "dynamic agent task ended");
        });
    }
}
