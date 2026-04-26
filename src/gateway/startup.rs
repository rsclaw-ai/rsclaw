//! Gateway startup orchestration.
//!
//! Wires together: config, store, providers, agent runtimes, channels,
//! cron scheduler, and HTTP server into a running gateway.

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::{
    MemoryTier,
    agent::{
        AgentMessage, AgentRegistry, AgentReply, AgentRuntime, AgentSpawner,
        MemoryStore, PendingAnalysis,
    },
    channel::OutboundMessage,
    config::{
        self,
        runtime::RuntimeConfig,
        schema::BindMode,
    },
    cron::CronRunner,
    gateway::{
        LiveConfig,
        hot_reload::{ConfigChange, FileWatcher},
    },
    plugin::{MemoryStoreSlot, PluginRegistry, load_all_plugins},
    provider::registry::ProviderRegistry,
    server::{AppState, serve},
    skill::{SkillRegistry, load_skills},
    store::Store,
};

use super::channels::{start_channels, start_custom_channels};
use super::providers::build_providers;

// ---------------------------------------------------------------------------
// Gateway entry point
// ---------------------------------------------------------------------------

/// Start the full gateway. Blocks until shutdown (Ctrl-C).
pub async fn start_gateway(config: Arc<RuntimeConfig>, tier: MemoryTier) -> Result<()> {
    // 0. Apply global proxy env vars before any HTTP clients are created.
    crate::config::apply_proxy_env(&config);

    // 1. Resolve data directory — respects RSCLAW_BASE_DIR for --dev/--profile.
    let base_dir = crate::config::loader::base_dir();
    let data_dir = base_dir.join("var/data");
    std::fs::create_dir_all(&data_dir).context("create data dir")?;

    // 1b. Seed tool prompts if not present.
    {
        let lang = config.raw.gateway.as_ref().and_then(|g| g.language.as_deref());
        if let Err(e) = crate::agent::bootstrap::seed_tools(&base_dir, lang) {
            warn!("failed to seed tool prompts: {e:#}");
        }
    }

    // 2. Open store. If the database is locked by another instance, exit cleanly
    //    so systemd won't keep restarting.
    let store = match Store::open(&data_dir, tier) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("already open") || msg.contains("Cannot acquire lock") {
                eprintln!("  [!] Database locked by another gateway instance. Exiting cleanly.");
                std::process::exit(0);
            }
            return Err(e).context("open store");
        }
    };
    info!("store opened at {}", data_dir.display());

    // 3. Build provider registry.
    let providers = Arc::new(build_providers(&config));
    info!("{} provider(s) registered", providers.names().len());

    // 4. Load skills.
    let global_skills = base_dir.join("skills");
    let skills = Arc::new(
        load_skills(&global_skills, None, config.ext.skills.as_ref()).unwrap_or_else(|e| {
            warn!("failed to load skills: {e:#}");
            SkillRegistry::new()
        }),
    );
    info!("{} skill(s) loaded", skills.len());

    // 5. Build agent registry with live receivers.
    let (registry, receivers) =
        AgentRegistry::from_config_with_receivers(&config, Arc::clone(&providers));
    let registry = Arc::new(registry);
    info!("{} agent(s) registered", registry.len());

    // Create notification broadcast channel early so background model downloads
    // can also send notifications to users via channels.
    let (notification_tx, notification_rx) =
        broadcast::channel::<crate::channel::OutboundMessage>(64);

    // Restart-required event channel + latch. Published into by the file
    // watcher bridge and the BGE auto-downloader; subscribed to by WS dispatch
    // so UI clients see banners. Allocated early so the BGE downloader (next
    // step) and the file-watcher bridge (later) can both publish.
    let (restart_request_tx, _restart_request_rx) =
        tokio::sync::broadcast::channel::<crate::events::RestartRequest>(16);
    let pending_restart: Arc<std::sync::RwLock<Option<crate::events::RestartRequest>>> =
        Arc::new(std::sync::RwLock::new(None));

    // 6. Open shared memory store.
    // Auto-detect embedding model: prefer higher-quality models first.
    // Priority: bge-base-zh > bge-small-zh > bge-small-en > auto-download small-zh.
    let search_cfg = config.raw.memory_search.as_ref();
    let model_dir = {
        let base_zh = base_dir.join("models/bge-base-zh");
        let zh = base_dir.join("models/bge-small-zh");
        let en = base_dir.join("models/bge-small-en");
        if base_zh.join("model.safetensors").exists() {
            base_zh
        } else if zh.join("model.safetensors").exists() {
            zh
        } else if en.join("model.safetensors").exists() {
            en
        } else {
            // Auto-download BGE model in background (don't block startup).
            let target_dir = zh; // default to small-zh
            let cfg_lang = config.raw.gateway.as_ref().and_then(|g| g.language.as_deref()).map(str::to_owned);
            let i18n_lang = crate::i18n::resolve_lang(cfg_lang.as_deref().unwrap_or("en")).to_owned();
            let search_cfg_clone = search_cfg.cloned();
            let dl_dir = target_dir.clone();
            let bge_restart_tx = restart_request_tx.clone();
            let bge_pending = Arc::clone(&pending_restart);
            tokio::spawn(async move {
                info!("BGE embedding model not found, downloading in background...");
                match download_bge_model(&dl_dir, search_cfg_clone.as_ref(), cfg_lang.as_deref()).await {
                    Ok(()) => {
                        info!("BGE model downloaded. Notifying UI to restart.");
                        publish_restart(
                            &bge_restart_tx,
                            &bge_pending,
                            crate::events::RestartRequest::new(
                                crate::events::RestartReason::ModelDownloaded {
                                    name: "bge-small-zh".into(),
                                },
                                crate::events::RestartUrgency::Recommended,
                                crate::i18n::t("restart_required_model_downloaded", &i18n_lang),
                            ),
                        );
                    }
                    Err(e) => {
                        warn!("BGE model auto-download failed: {e:#}");
                        // Failure is reported via the same banner channel so the
                        // user sees it in the UI; urgency is Recommended (the
                        // gateway works without semantic search).
                        publish_restart(
                            &bge_restart_tx,
                            &bge_pending,
                            crate::events::RestartRequest::new(
                                crate::events::RestartReason::ModelDownloadFailed {
                                    name: "bge-small-zh".into(),
                                    error: format!("{e:#}"),
                                },
                                crate::events::RestartUrgency::Recommended,
                                crate::i18n::t_fmt(
                                    "restart_model_download_failed",
                                    &i18n_lang,
                                    &[("error", &format!("{e:#}"))],
                                ),
                            ),
                        );
                    }
                }
            });
            target_dir
        }
    };
    let memory = match MemoryStore::open(&data_dir, Some(&model_dir), tier, search_cfg).await {
        Ok(m) => {
            info!("memory store opened");
            Some(Arc::new(tokio::sync::Mutex::new(m)))
        }
        Err(e) => {
            warn!("failed to open memory store: {e:#}");
            None
        }
    };

    // 7. Load all plugins (JS + WASM) and register built-in memory slot.
    let plugins_dir = base_dir.join("plugins");
    let wasm_browser: Arc<tokio::sync::Mutex<Option<crate::browser::BrowserSession>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let mut plugin_registry = load_all_plugins(
        &plugins_dir,
        config.ext.plugins.as_ref(),
        Arc::clone(&wasm_browser),
    )
    .await
    .unwrap_or_else(|e| {
        warn!("plugin load error: {e:#}");
        PluginRegistry::new()
    });
    if let Some(ref mem_arc) = memory
        && !plugin_registry.slots.has_memory()
    {
        let slot = MemoryStoreSlot::new(Arc::clone(mem_arc));
        let _ = plugin_registry.slots.set_memory(Arc::new(slot), "built-in");
    }
    info!(
        "{} plugin(s) loaded (js={}, wasm={}), memory slot: {}",
        plugin_registry.len(),
        plugin_registry.js_count(),
        plugin_registry.wasm_count(),
        plugin_registry.slots.has_memory()
    );

    let wasm_plugins = Arc::new(plugin_registry.take_wasm_plugins());
    let plugins = Arc::new(plugin_registry);

    // Create the SSE broadcast channel once so agents and the HTTP server
    // share the same sender.
    let (event_tx, _) = broadcast::channel::<crate::events::AgentEvent>(1024);

    // Create AgentSpawner — enables agent-to-agent dynamic spawning.
    let spawner = AgentSpawner::new_arc(
        Arc::clone(&registry),
        Arc::clone(&config),
        Arc::clone(&providers),
        Arc::clone(&skills),
        Arc::clone(&store),
        memory.clone(),
        event_tx.clone(),
        Some(Arc::clone(&plugins)),
    );

    // Spawn MCP servers and discover tools (before agent tasks so tools are
    // available).
    let mcp_registry = Arc::new(crate::mcp::McpRegistry::new());
    spawn_mcp_servers(&config, Arc::clone(&mcp_registry)).await;

    // Clone memory before passing to agent tasks so heartbeat can also use it.
    let heartbeat_memory = memory.clone();

    spawn_agent_tasks(
        receivers,
        Arc::clone(&registry),
        Arc::clone(&config),
        Arc::clone(&store),
        Arc::clone(&skills),
        Arc::clone(&providers),
        memory,
        event_tx.clone(),
        Some(Arc::clone(&spawner)),
        Some(Arc::clone(&plugins)),
        Some(Arc::clone(&mcp_registry)),
        Some(notification_tx.clone()),
        Arc::clone(&wasm_plugins),
    );

    // Set i18n default language from gateway config.
    let lang = config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.language.as_deref());
    info!(lang = ?lang, "i18n: gateway language config");
    if let Some(lang) = lang {
        crate::i18n::set_default_lang(lang);
        info!(
            resolved = crate::i18n::default_lang(),
            "i18n: default language set"
        );
    }

    // 8. Build channel manager and start channels.
    let mut channel_manager = crate::channel::ChannelManager::new(tier);
    let feishu_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::feishu::FeishuChannel>>> =
        Arc::new(tokio::sync::OnceCell::new());
    let wecom_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::wecom::WeComChannel>>> =
        Arc::new(tokio::sync::OnceCell::new());
    let whatsapp_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::whatsapp::WhatsAppChannel>>> =
        Arc::new(tokio::sync::OnceCell::new());
    let line_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::line::LineChannel>>> =
        Arc::new(tokio::sync::OnceCell::new());
    let zalo_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::zalo::ZaloChannel>>> =
        Arc::new(tokio::sync::OnceCell::new());
    let dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    > = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

    // Channel sender registry for notification routing.
    let channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    > = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

    // Create task queue manager before channels so channels can submit to it.
    let task_queue_mgr = Arc::new(
        super::task_queue::TaskQueueManager::new(Arc::clone(&store.db)),
    );

    start_channels(
        &config,
        Arc::clone(&registry),
        &mut channel_manager,
        Arc::clone(&feishu_slot),
        Arc::clone(&wecom_slot),
        Arc::clone(&whatsapp_slot),
        Arc::clone(&line_slot),
        Arc::clone(&zalo_slot),
        Arc::clone(&dm_enforcers),
        Arc::clone(&store.db),
        Arc::clone(&channel_senders),
        Arc::clone(&task_queue_mgr),
    );

    // Graceful-shutdown coordinator — wired to task queue worker, axum graceful
    // shutdown, and the /api/v1/restart drain handler.
    let shutdown = crate::gateway::ShutdownCoordinator::new();

    // Spawn task queue worker — processes queued tasks in priority order.
    {
        let worker = Arc::new(super::task_queue::TaskQueueWorker::new(
            Arc::clone(&task_queue_mgr),
            Arc::clone(&registry),
            Arc::clone(&channel_senders),
            shutdown.clone(),
            (*config).clone(),
        ));
        tokio::spawn(async move { worker.run().await });
        info!("task queue worker started");
    }

    // Spawn notification router task — routes OutboundMessages from ACP tools
    // (OpenCode, ClaudeCode) to the correct channel based on msg.channel.
    {
        let senders = Arc::clone(&channel_senders);
        let mut rx = notification_rx;
        tokio::spawn(async move {
            info!("notification router started");
            while let Ok(msg) = rx.recv().await {
                if let Some(ref ch_name) = msg.channel {
                    // Get sender BEFORE any await — drop guard immediately after cloning sender
                    let tx = {
                        let senders_guard = senders.read().expect("channel_senders RwLock poisoned");
                        senders_guard.get(ch_name).cloned()
                    };
                    if let Some(tx) = tx {
                        info!(channel = %ch_name, target_id = %msg.target_id, "routing notification");
                        if let Err(e) = tx.send(msg.clone()).await {
                            tracing::warn!(error = %e, "notification send failed");
                        }
                    } else {
                        warn!(channel = %ch_name, "no channel sender registered for notification");
                    }
                } else {
                    // No channel specified — send to first registered channel (default)
                    let first = {
                        let guard = senders.read().expect("channel_senders RwLock poisoned");
                        guard.iter().next().map(|(k, v)| (k.clone(), v.clone()))
                    };
                    if let Some((ch_name, tx)) = first {
                        info!(channel = %ch_name, "routing notification to default channel");
                        if let Err(e) = tx.send(msg.clone()).await {
                            tracing::warn!(error = %e, "notification send failed");
                        }
                    } else {
                        warn!("notification: no channels registered");
                    }
                }
            }
            info!("notification router ended");
        });
    }

    // 9. Start heartbeat runner — scans agent workspaces for HEARTBEAT.md.
    let hb_enabled = config
        .agents
        .defaults
        .heartbeat
        .as_ref()
        .and_then(|h| h.enabled)
        .unwrap_or(true);
    if hb_enabled {
        let runner = std::sync::Arc::new(crate::heartbeat::HeartbeatRunner::new_with_shutdown(
            Arc::clone(&registry),
            &data_dir,
            heartbeat_memory,
            Some(shutdown.clone()),
        ));
        runner.run();
        info!("heartbeat runner started");
    }

    // 11. Build LiveConfig for hot-reloadable per-domain access.
    let live = Arc::new(LiveConfig::new((*config).clone()));

    // 11. Write PID file early so the hot-reload task can clean it on restart.
    let pid_file = crate::config::loader::pid_file();
    if let Some(parent) = pid_file.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!("could not create PID file directory: {e}");
        }
    }
    let pid = std::process::id();
    if let Err(e) = std::fs::write(&pid_file, pid.to_string()) {
        warn!("could not write PID file: {e}");
    }
    info!(pid, "gateway PID written to {}", pid_file.display());

    // 12. Start config hot-reload watcher (if config file is detectable).
    if let Some(config_path) = config::loader::detect_config_path() {
        let (mut watcher, mut reload_rx) = FileWatcher::new(config_path);
        tokio::spawn(async move { watcher.run().await });
        let live_reload = Arc::clone(&live);
        let (restart_tx, _) = broadcast::channel::<Vec<String>>(8);
        let bridge_tx = restart_request_tx.clone();
        let bridge_pending = Arc::clone(&pending_restart);
        let cfg_lang = config
            .raw
            .gateway
            .as_ref()
            .and_then(|g| g.language.as_deref())
            .map(str::to_owned);
        tokio::spawn(async move {
            let lang = crate::i18n::resolve_lang(cfg_lang.as_deref().unwrap_or("en")).to_owned();
            loop {
                match reload_rx.recv().await {
                    Ok(ConfigChange::FullReload(new_cfg)) => {
                        let needs_restart =
                            live_reload.apply((*new_cfg).clone(), &restart_tx).await;
                        if needs_restart.is_empty() {
                            info!("config hot-reload applied");
                        } else {
                            warn!(?needs_restart, "config change requires gateway restart");
                        }
                        // Even FullReload doesn't fully propagate to running
                        // agents (their providers/prompts are snapshotted at
                        // spawn). Surface a Recommended restart banner so the
                        // user can choose to apply changes cleanly.
                        publish_restart(
                            &bridge_tx,
                            &bridge_pending,
                            crate::events::RestartRequest::new(
                                crate::events::RestartReason::ConfigChanged {
                                    sections: needs_restart,
                                },
                                crate::events::RestartUrgency::Recommended,
                                crate::i18n::t("restart_required_config_changed", &lang),
                            ),
                        );
                    }
                    Ok(ConfigChange::RequiresRestart(fields)) => {
                        warn!(?fields, "config change requires restart — surfacing banner");
                        publish_restart(
                            &bridge_tx,
                            &bridge_pending,
                            crate::events::RestartRequest::new(
                                crate::events::RestartReason::ConfigChanged {
                                    sections: fields,
                                },
                                crate::events::RestartUrgency::Required,
                                crate::i18n::t("restart_required_config_changed", &lang),
                            ),
                        );
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
    }

    // 13. Start HTTP server.
    let devices_path = crate::config::loader::base_dir().join("var/data/devices.json");
    let devices = Arc::new(crate::ws::DeviceStore::new(devices_path));
    let ws_conns = Arc::new(crate::ws::ConnRegistry::new());

    // Start custom channels (webhook + websocket).
    let custom_webhooks: Arc<
        std::sync::RwLock<
            std::collections::HashMap<String, Arc<crate::channel::custom::CustomWebhookChannel>>,
        >,
    > = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    start_custom_channels(
        &config,
        Arc::clone(&registry),
        &mut channel_manager,
        Arc::clone(&custom_webhooks),
    );

    // Register desktop channel — routes cron delivery to connected WS clients.
    {
        let desktop_ch = Arc::new(crate::channel::desktop::DesktopChannel::new(Arc::clone(&ws_conns)));
        // Bridge the notification_tx → DesktopChannel path so AgentRuntime
        // (which only has notification_tx, not ChannelManager) can route
        // short-delay reminders through the same broadcast path cron uses.
        let (desktop_out_tx, mut desktop_out_rx) = mpsc::channel::<OutboundMessage>(64);
        {
            let mut senders = channel_senders
                .write()
                .expect("channel_senders lock poisoned");
            senders.insert("desktop".to_string(), desktop_out_tx.clone());
            // "ws" is the channel name used by WS-originated agent runs (see
            // ws/methods/chat.rs where AgentMessage.channel = "ws"). Without
            // this alias, OutboundMessages tagged channel="ws" — e.g. WASM
            // plugin progress notify(), async task completion messages — hit
            // the notification router's "no channel sender registered" warn
            // and get dropped, leaving the desktop UI without progress pings.
            senders.insert("ws".to_string(), desktop_out_tx);
        }
        let desktop_for_bridge = Arc::clone(&desktop_ch);
        tokio::spawn(async move {
            use crate::channel::Channel;
            while let Some(msg) = desktop_out_rx.recv().await {
                if let Err(e) = desktop_for_bridge.send(msg).await {
                    warn!(error = %e, "desktop notification bridge: send failed");
                }
            }
        });
        if let Err(e) = channel_manager.register(desktop_ch as Arc<dyn crate::channel::Channel>) {
            warn!("failed to register desktop channel: {e}");
        }
    }

    // All channels registered - now wrap for sharing with cron runner
    let channel_manager = Arc::new(channel_manager);

    // Create cron reload broadcast channel (used to notify CronRunner of new jobs)
    let (cron_reload_tx, _cron_reload_rx) = tokio::sync::broadcast::channel::<()>(16);

    // Start cron runner — jobs loaded from base_dir/cron.json5
    {
        let cron_cfg = config.ops.cron.clone().unwrap_or_else(|| {
            crate::config::schema::CronConfig {
                enabled: Some(true),
                max_concurrent_runs: None,
                session_retention: None,
                run_log: None,
                jobs: None,
                default_delivery: None,
            }
        });
        let cron_enabled = cron_cfg.enabled.unwrap_or(true);

        // Load jobs from openclaw-compatible path
        let cron_file = crate::cron::resolve_cron_store_path();
        let jobs = crate::cron::load_cron_jobs();
        if !jobs.is_empty() {
            info!(file = %cron_file.display(), count = jobs.len(), "loaded cron jobs");
        }

        if cron_enabled {
            let cron_data_dir = base_dir.join("var").join("data");
            let runner = CronRunner::new_with_shutdown(
                &cron_cfg,
                jobs,
                Arc::clone(&registry),
                Arc::clone(&channel_manager),
                cron_data_dir,
                cron_reload_tx.clone(),
                Arc::clone(&ws_conns),
                Some(shutdown.clone()),
            );
            tokio::spawn(async move {
                if let Err(e) = runner.run().await {
                    error!("cron runner error: {e:#}");
                }
            });
            info!("cron runner started");
        }
    }

    let state = AppState {
        config: Arc::clone(&config),
        live: Arc::clone(&live),
        agents: Arc::clone(&registry),
        store: Arc::clone(&store),
        event_bus: event_tx,
        devices,
        ws_conns,
        feishu: Arc::clone(&feishu_slot),
        wecom: Arc::clone(&wecom_slot),
        whatsapp: Arc::clone(&whatsapp_slot),
        line: Arc::clone(&line_slot),
        zalo: Arc::clone(&zalo_slot),
        started_at: std::time::Instant::now(),
        dm_enforcers: Arc::clone(&dm_enforcers),
        custom_webhooks: Arc::clone(&custom_webhooks),
        cron_reload: cron_reload_tx,
        notification_tx: notification_tx.clone(),
        wasm_plugins: Arc::clone(&wasm_plugins),
        restart_request_tx: restart_request_tx.clone(),
        pending_restart: Arc::clone(&pending_restart),
        shutdown: shutdown.clone(),
    };
    crate::ws::tick::start_tick_loop(Arc::clone(&state.ws_conns));

    // Start browser pool idle reaper (checks every 60s).
    tokio::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            crate::browser::pool::BrowserPool::global().reap_if_idle().await;
        }
    });

    let bind_addr = resolve_bind_addr(&config);
    info!("starting HTTP server on {bind_addr}");

    // Background update check (non-blocking)
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let client = reqwest::Client::builder()
            .user_agent("rsclaw/dev")
            .timeout(std::time::Duration::from_secs(10))
            .build();

        let Ok(client) = client else { return };

        let resp = client
            .get("https://api.github.com/repos/rsclaw-ai/rsclaw/releases/latest")
            .send()
            .await;

        if let Ok(resp) = resp {
            if let Ok(release) = resp.json::<serde_json::Value>().await {
                let latest_raw = release["tag_name"]
                    .as_str()
                    .unwrap_or("");
                let current_raw = option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev");
                // Extract bare version: "2026.4.1 (abc123)" -> "2026.4.1",
                // "2026.4.1-beta" -> "2026.4.1".
                fn strip_ver(s: &str) -> &str {
                    let s = s.trim_start_matches('v');
                    let s = s.split_once(' ').map(|(v, _)| v).unwrap_or(s);
                    s.split_once('-').map(|(v, _)| v).unwrap_or(s)
                }
                fn ver_newer(latest: &str, current: &str) -> bool {
                    let parse = |s: &str| -> Vec<u32> {
                        s.split('.').filter_map(|p| p.parse().ok()).collect()
                    };
                    let l = parse(latest);
                    let c = parse(current);
                    for i in 0..l.len().max(c.len()) {
                        let lv = l.get(i).copied().unwrap_or(0);
                        let cv = c.get(i).copied().unwrap_or(0);
                        if lv > cv {
                            return true;
                        }
                        if lv < cv {
                            return false;
                        }
                    }
                    false
                }
                let latest = strip_ver(latest_raw);
                let current = strip_ver(current_raw);
                if !latest.is_empty() && ver_newer(latest, current) {
                    info!(
                        current = current_raw,
                        latest = latest_raw,
                        "new rsclaw version available -- run `rsclaw update` to upgrade"
                    );
                }
            }
        }
    });

    let result = serve(state, bind_addr).await;

    // At this point `axum::serve` has returned, which means the listener has
    // been dropped — so the port is free for whatever runs next. Two paths:
    //   - clean shutdown (Ctrl-C, SIGTERM, /api/v1/shutdown): just clean up
    //     the PID file and return.
    //   - restart requested (/api/v1/restart, system.restart): wait for
    //     non-HTTP inflight to drain, spawn the replacement, then exit.
    //     We spawn HERE rather than in the restart handler to avoid the
    //     race where the child's `bind()` runs before the parent's listener
    //     drops; that race could cause `cmd_gateway` to see "port in use"
    //     and exit cleanly, leaving the gateway dead.
    if shutdown.is_restart_requested() {
        info!("restart requested - waiting for inflight drain (max 60s)");
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let n = shutdown.inflight();
            if n == 0 {
                info!("graceful drain: inflight cleared");
                break;
            }
            if std::time::Instant::now() >= deadline {
                warn!(
                    inflight = n,
                    "graceful drain: 60s timeout reached, restarting anyway"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                error!("current_exe failed; cannot respawn replacement: {e:#}");
                // Don't remove the PID file - we'd rather leave a stale PID
                // than wipe it and bail with no replacement running.
                return result;
            }
        };
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["gateway", "run"]);
        // Windows: suppress the console flash when re-execing from a GUI app.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.spawn() {
            Ok(_) => info!("replacement gateway spawned"),
            Err(e) => error!("failed to spawn replacement gateway: {e:#}"),
        }
        // Do NOT remove the PID file - the new gateway process overwrites it
        // with its own PID on startup. Removing here races and can leave us
        // with no PID file after a successful restart.
        std::process::exit(0);
    }

    // Clean shutdown path - remove the PID file before returning.
    if let Err(e) = std::fs::remove_file(&pid_file) {
        warn!("could not remove PID file on exit: {e}");
    }
    result
}


// ---------------------------------------------------------------------------
// Agent task spawning
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn spawn_agent_tasks(
    receivers: HashMap<String, mpsc::Receiver<AgentMessage>>,
    registry: Arc<AgentRegistry>,
    config: Arc<RuntimeConfig>,
    store: Arc<Store>,
    skills: Arc<SkillRegistry>,
    providers: Arc<ProviderRegistry>,
    memory: Option<Arc<tokio::sync::Mutex<MemoryStore>>>,
    event_tx: broadcast::Sender<crate::events::AgentEvent>,
    spawner: Option<Arc<AgentSpawner>>,
    plugins: Option<Arc<crate::plugin::PluginRegistry>>,
    mcp: Option<Arc<crate::mcp::McpRegistry>>,
    notification_tx: Option<broadcast::Sender<crate::channel::OutboundMessage>>,
    wasm_plugins: Arc<Vec<crate::plugin::WasmPlugin>>,
) {
    for (agent_id, mut rx) in receivers {
        let handle = match registry.get(&agent_id) {
            Ok(h) => h,
            Err(e) => {
                error!(agent_id, "agent handle not found: {e:#}");
                continue;
            }
        };

        // Collect fallback models from agent config → global defaults.
        let fallback_models = handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.fallbacks.clone())
            .or_else(|| {
                config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.fallbacks.clone())
            })
            .unwrap_or_default();

        let mut runtime = AgentRuntime::new(
            Arc::clone(&handle),
            Arc::clone(&config),
            Arc::clone(&providers),
            fallback_models,
            Arc::clone(&skills),
            Arc::clone(&store),
            memory.clone(),
            Some(Arc::clone(&registry)),
            Some(event_tx.clone()),
            spawner.clone(),
            plugins.clone(),
            mcp.clone(),
            notification_tx.clone(),
        );

        // Inject WASM plugins into the agent runtime.
        runtime.wasm_plugins = Arc::clone(&wasm_plugins);

        let event_tx_task = event_tx.clone();
        tokio::spawn(async move {
            info!(agent_id = %handle.id, "agent runtime task started");
            while let Some(msg) = rx.recv().await {
                info!(
                    agent_id = %handle.id,
                    session_key = %msg.session_key,
                    channel = %msg.channel,
                    "agent runtime: received msg from queue"
                );
                let AgentMessage {
                    session_key,
                    text,
                    channel,
                    peer_id,
                    chat_id: _,
                    reply_tx,
                    extra_tools,
                    images,
                    files,
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
                let turn_errored = result.is_err();
                let reply = result.unwrap_or_else(|e| {
                    error!(agent = %handle.id, "turn error: {e:#}");
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
                // Emit to event_bus for preparse turns (agent_loop already
                // emits streaming deltas + done for LLM turns, so a second
                // emit would duplicate the done frame) *and* for turns that
                // failed with Err (agent_loop returns early via `?` on LLM
                // errors and never gets to emit done — WS clients would hang
                // waiting for the terminator forever).
                if reply.was_preparse || turn_errored {
                    if !reply.text.is_empty() {
                        // receiver may have been dropped
                        let _ = event_tx_task.send(crate::events::AgentEvent {
                            session_id: session_key.clone(),
                            agent_id: handle.id.clone(),
                            delta: reply.text.clone(),
                            done: false,
                            files: vec![],
                            images: vec![],
                            tool_log: vec![],
                        });
                    }
                    // receiver may have been dropped
                    let _ = event_tx_task.send(crate::events::AgentEvent {
                        session_id: session_key.clone(),
                        agent_id: handle.id.clone(),
                        delta: String::new(),
                        done: true,
                        files: vec![],
                        images: vec![],
                        tool_log: vec![],
                    });
                }
                // receiver may have been dropped (e.g. channel timeout)
                let _ = reply_tx.send(reply);
            }
            info!(agent_id = %handle.id, "agent runtime task ended (channel closed)");
        });
    }
}

// ---------------------------------------------------------------------------
// Bind address helper
// ---------------------------------------------------------------------------

fn resolve_bind_addr(config: &RuntimeConfig) -> SocketAddr {
    let port = config.gateway.port;
    // If a custom bind_address is set, parse and use it.
    if let Some(ref addr) = config.gateway.bind_address {
        if let Ok(ip) = addr.parse::<std::net::IpAddr>() {
            return SocketAddr::new(ip, port);
        }
        tracing::warn!(
            addr = addr.as_str(),
            "invalid bind_address, falling back to bind mode"
        );
    }
    match config.gateway.bind {
        BindMode::Auto | BindMode::Lan => SocketAddr::from(([0, 0, 0, 0], port)),
        BindMode::Loopback => SocketAddr::from(([127, 0, 0, 1], port)),
        BindMode::All => SocketAddr::from(([0, 0, 0, 0], port)),
        BindMode::Custom => SocketAddr::from(([0, 0, 0, 0], port)),
        BindMode::Tailnet => SocketAddr::from(([127, 0, 0, 1], port)),
    }
}

// ---------------------------------------------------------------------------
// MCP server process management
// ---------------------------------------------------------------------------

async fn spawn_mcp_servers(config: &RuntimeConfig, registry: Arc<crate::mcp::McpRegistry>) {
    let mcp = match config.raw.mcp.as_ref() {
        Some(m) => m,
        None => return,
    };

    if mcp.enabled == Some(false) {
        return;
    }

    let servers = match mcp.servers.as_ref() {
        Some(s) => s,
        None => return,
    };

    for server_cfg in servers {
        match crate::mcp::McpClient::spawn(server_cfg).await {
            Ok(mut client) => {
                // Initialize + discover tools.
                if let Err(e) = client.initialize().await {
                    error!(name = %server_cfg.name, error = %e, "MCP initialize failed");
                    continue;
                }
                match client.list_tools().await {
                    Ok(tools) => {
                        info!(
                            name = %server_cfg.name,
                            tools = tools.len(),
                            "MCP server ready"
                        );
                    }
                    Err(e) => {
                        warn!(name = %server_cfg.name, error = %e, "MCP tools/list failed");
                    }
                }
                registry.register(Arc::new(client)).await;
            }
            Err(e) => {
                error!(name = %server_cfg.name, error = %e, "failed to start MCP server");
            }
        }
    }

    let total = registry.clients.lock().await.len();
    if total > 0 {
        info!(count = total, "MCP server(s) registered");
    }
}

// ---------------------------------------------------------------------------
// QQ Official Bot (QQ机器人)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Pending file analysis helper
// ---------------------------------------------------------------------------

/// Process a pending file analysis: send the analysis text to the agent for
/// LLM processing and deliver the result (or timeout/error message) as a
/// follow-up outbound message.
pub(crate) async fn handle_pending_analysis(
    analysis: PendingAnalysis,
    handle: Arc<crate::agent::AgentHandle>,
    out_tx: &mpsc::Sender<crate::channel::OutboundMessage>,
    target_id: String,
    is_group: bool,
    config: &RuntimeConfig,
) {
    let i18n_lang = config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.language.as_deref())
        .map(crate::i18n::resolve_lang)
        .unwrap_or("en");

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: analysis.session_key,
        text: analysis.text,
        channel: analysis.channel,
        peer_id: analysis.peer_id.clone(),
        chat_id: String::new(),
        reply_tx,
        extra_tools: vec![],
        images: vec![],
        files: vec![],
    };
    if handle.tx.send(msg).await.is_err() {
        // receiver may have been dropped
        let _ = out_tx
            .send(crate::channel::OutboundMessage {
                target_id,
                is_group,
                text: crate::i18n::t("analysis_failed", i18n_lang),
                reply_to: None,
                images: vec![],
                channel: None,

                    files: vec![],            })
            .await;
        return;
    }
    match tokio::time::timeout(Duration::from_secs(600), reply_rx).await {
        Ok(Ok(r)) if !r.text.is_empty() || !r.images.is_empty() || !r.files.is_empty() => {
            // receiver may have been dropped
            let _ = out_tx
                .send(crate::channel::OutboundMessage {
                    target_id,
                    is_group,
                    text: r.text,
                    reply_to: None,
                    images: r.images,
                    files: r.files,
                    channel: None,                })
                .await;
        }
        Ok(Ok(_)) => {} // empty reply, nothing to send
        Ok(Err(_)) => {
            // receiver may have been dropped
            let _ = out_tx
                .send(crate::channel::OutboundMessage {
                    target_id,
                    is_group,
                    text: crate::i18n::t("analysis_failed", i18n_lang),
                    reply_to: None,
                    images: vec![],
                    channel: None,

                    files: vec![],                })
                .await;
        }
        Err(_) => {
            // receiver may have been dropped
            let _ = out_tx
                .send(crate::channel::OutboundMessage {
                    target_id,
                    is_group,
                    text: crate::i18n::t("analysis_timeout", i18n_lang),
                    reply_to: None,
                    images: vec![],
                    channel: None,

                    files: vec![],                })
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// BGE model auto-download
// ---------------------------------------------------------------------------

/// Download BGE-Small embedding model files from HuggingFace.
/// Uses hf-mirror.com for Chinese locale, huggingface.co otherwise.
async fn download_bge_model(
    target_dir: &std::path::Path,
    search_cfg: Option<&crate::config::schema::MemorySearchConfig>,
    config_language: Option<&str>,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let local_cfg = search_cfg.and_then(|c| c.local.as_ref());

    // Determine model repo.
    let repo = local_cfg
        .and_then(|c| c.model_repo.as_deref())
        .unwrap_or("BAAI/bge-small-zh-v1.5");

    // Determine base URL: config override > locale auto-detect.
    let base_url = if let Some(url) = local_cfg.and_then(|c| c.model_download_url.as_deref()) {
        // User provided full URL — use as-is.
        url.trim_end_matches('/').to_owned()
    } else {
        // Auto-detect: config language > LANG/LC_ALL env var.
        let is_zh = config_language
            .map(|l| l.to_lowercase().contains("chinese") || l.to_lowercase().contains("zh"))
            .unwrap_or_else(|| {
                std::env::var("LANG")
                    .or_else(|_| std::env::var("LC_ALL"))
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains("zh")
            });
        let host = if is_zh { "https://hf-mirror.com" } else { "https://huggingface.co" };
        format!("{host}/{repo}/resolve/main")
    };

    let files = ["config.json", "model.safetensors", "tokenizer.json"];

    info!("downloading BGE embedding model from {base_url} ...");
    std::fs::create_dir_all(target_dir)?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()?;

    for filename in &files {
        let url = format!("{base_url}/{filename}");
        let dest = target_dir.join(filename);
        let partial = target_dir.join(format!("{filename}.partial"));

        if dest.exists() {
            debug!("{filename} already exists, skipping");
            continue;
        }

        // Retry up to 3 times with resume support.
        let mut success = false;
        for attempt in 0..3 {
            if attempt > 0 {
                info!("  retrying {filename} (attempt {})...", attempt + 1);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }

            // Check if partial file exists for resume.
            let existing_len = tokio::fs::metadata(&partial).await.map(|m| m.len()).unwrap_or(0);

            let mut req = client.get(&url);
            if existing_len > 0 {
                info!("  resuming {filename} from byte {existing_len}...");
                req = req.header("Range", format!("bytes={existing_len}-"));
            } else {
                info!("  downloading {filename} ...");
            }

            let response = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("  download request failed: {e}");
                    continue;
                }
            };

            let status = response.status();
            if !status.is_success() && status.as_u16() != 206 {
                warn!("  download failed: HTTP {status}");
                continue;
            }

            // Stream to partial file (append if resuming).
            let mut file = if existing_len > 0 && status.as_u16() == 206 {
                tokio::fs::OpenOptions::new().append(true).open(&partial).await?
            } else {
                tokio::fs::File::create(&partial).await?
            };

            let mut stream = response.bytes_stream();
            use futures::StreamExt;
            let mut downloaded = existing_len;
            let mut stream_ok = true;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        file.write_all(&bytes).await?;
                        downloaded += bytes.len() as u64;
                    }
                    Err(e) => {
                        warn!("  stream error at byte {downloaded}: {e}");
                        stream_ok = false;
                        break;
                    }
                }
            }
            file.flush().await?;

            if stream_ok {
                // Rename partial to final.
                tokio::fs::rename(&partial, &dest).await?;
                info!("  {filename} downloaded ({downloaded} bytes)");
                success = true;
                break;
            }
        }

        if !success {
            // Clean up partial file on final failure (best-effort, about to bail anyway).
            if let Err(e) = tokio::fs::remove_file(&partial).await {
                warn!("could not remove partial download file: {e}");
            }
            anyhow::bail!("failed to download {filename} after 3 attempts");
        }
    }

    info!("BGE model downloaded to {}", target_dir.display());
    Ok(())
}

/// Publish a `RestartRequest` into the broadcast channel and store it in the
/// `pending_restart` latch so late-connecting UI clients see it on handshake.
///
/// `send` failure (no live subscribers) is normal and ignored — the latch
/// guarantees the next subscriber will pick it up.
pub(crate) fn publish_restart(
    tx: &tokio::sync::broadcast::Sender<crate::events::RestartRequest>,
    latch: &Arc<std::sync::RwLock<Option<crate::events::RestartRequest>>>,
    req: crate::events::RestartRequest,
) {
    if let Ok(mut guard) = latch.write() {
        *guard = Some(req.clone());
    } else {
        warn!("pending_restart lock poisoned; restart event still broadcast");
    }
    let _ = tx.send(req);
}
