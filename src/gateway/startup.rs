//! Gateway startup orchestration.
//!
//! Wires together: config, store, providers, agent runtimes, channels,
//! cron scheduler, and HTTP server into a running gateway.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use futures::StreamExt as _;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::{
    MemoryTier,
    agent::{
        AgentMessage, AgentRegistry, AgentReply, AgentRuntime, AgentSpawner, LiveStatus,
        MemoryStore, PendingAnalysis,
    },
channel::{
        Channel, OutboundMessage, cli::CliChannel, telegram::TelegramChannel,
    },
    config::{
        self,
        runtime::RuntimeConfig,
        schema::{BindMode, DmScope},
    },
    cron::CronRunner,
    gateway::{
        LiveConfig,
        hot_reload::{ConfigChange, FileWatcher},
        session::{MessageKind, SessionKeyParams, derive_session_key},
    },
    plugin::{MemoryStoreSlot, PluginRegistry, load_plugins},
    provider::{
        LlmProvider, LlmRequest, Message, MessageContent, Role, StreamEvent,
        anthropic::{self as anthropic, AnthropicProvider},
        failover::FailoverManager,
        gemini::{self as gemini, GeminiProvider},
        openai::OpenAiProvider,
        registry::ProviderRegistry,
    },
    server::{AppState, serve},
    skill::{SkillRegistry, load_skills},
    store::Store,
};

// ---------------------------------------------------------------------------
// Gateway entry point
// ---------------------------------------------------------------------------

/// Start the full gateway. Blocks until shutdown (Ctrl-C).
pub async fn start_gateway(config: Arc<RuntimeConfig>, tier: MemoryTier) -> Result<()> {
    // 1. Resolve data directory — respects RSCLAW_BASE_DIR for --dev/--profile.
    let base_dir = crate::config::loader::base_dir();
    let data_dir = base_dir.join("var/data");
    std::fs::create_dir_all(&data_dir).context("create data dir")?;

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

    // 6. Open shared memory store.
    // Auto-detect embedding model: prefer bge-small-zh, fallback to bge-small-en.
    // Auto-download if neither exists.
    let search_cfg = config.raw.memory_search.as_ref();
    let model_dir = {
        let zh = base_dir.join("models/bge-small-zh");
        let en = base_dir.join("models/bge-small-en");
        if zh.join("config.json").exists() {
            zh
        } else if en.join("config.json").exists() {
            en
        } else {
            // Auto-download BGE model.
            let target_dir = zh; // default to zh
            if let Err(e) = download_bge_model(&target_dir, search_cfg).await {
                warn!("BGE model auto-download failed: {e:#}");
                warn!("semantic memory search will use FNV fallback (low quality)");
                warn!("to fix: manually download BGE model or configure memory.search.provider");
            }
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

    // 7. Load plugins and register built-in memory slot.
    let plugins_dir = base_dir.join("plugins");
    let mut plugin_registry = load_plugins(&plugins_dir, config.ext.plugins.as_ref(), None)
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
        "{} plugin(s) loaded, memory slot: {}",
        plugin_registry.len(),
        plugin_registry.slots.has_memory()
    );

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

    // Create notification broadcast channel for routing OutboundMessages from
    // ACP tools (OpenCode, ClaudeCode) back to the correct channel.
    let (notification_tx, notification_rx) =
        broadcast::channel::<crate::channel::OutboundMessage>(64);

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
    );

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
                        let senders_guard = senders.read().unwrap();
                        senders_guard.get(ch_name).cloned()
                    };
                    if let Some(tx) = tx {
                        info!(channel = %ch_name, target_id = %msg.target_id, "routing notification");
                        let _ = tx.send(msg.clone()).await;
                    } else {
                        warn!(channel = %ch_name, "no channel sender registered for notification");
                    }
                } else {
                    warn!("notification message has no channel field, cannot route");
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
        let runner = std::sync::Arc::new(crate::heartbeat::HeartbeatRunner::new(
            Arc::clone(&registry),
            &data_dir,
        ));
        runner.run();
        info!("heartbeat runner started");
    }

    // 11. Build LiveConfig for hot-reloadable per-domain access.
    let live = Arc::new(LiveConfig::new((*config).clone()));

    // 11. Write PID file early so the hot-reload task can clean it on restart.
    let pid_file = crate::config::loader::pid_file();
    if let Some(parent) = pid_file.parent() {
        let _ = std::fs::create_dir_all(parent);
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
        let pid_file_restart = pid_file.clone();
        let (restart_tx, _) = broadcast::channel::<Vec<String>>(8);
        tokio::spawn(async move {
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
                    }
                    Ok(ConfigChange::RequiresRestart(fields)) => {
                        warn!(
                            ?fields,
                            "config change requires restart — restarting gateway"
                        );
                        let _ = std::fs::remove_file(&pid_file_restart);
                        match std::env::current_exe() {
                            Ok(exe) => {
                                let args: Vec<String> = std::env::args().skip(1).collect();
                                match std::process::Command::new(&exe).args(&args).spawn() {
                                    Ok(_) => std::process::exit(0),
                                    Err(e) => error!("failed to re-exec gateway: {e:#}"),
                                }
                            }
                            Err(e) => error!("cannot resolve current exe for restart: {e:#}"),
                        }
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

    // All channels registered - now wrap for sharing with cron runner
    let channel_manager = Arc::new(channel_manager);

    // Create cron reload broadcast channel (used to notify CronRunner of new jobs)
    let (cron_reload_tx, _cron_reload_rx) = tokio::sync::broadcast::channel::<()>(16);

    // Start cron runner — jobs loaded from base_dir/cron/jobs.json
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
            // Use openclaw-compatible path for cron run logs (respects OPENCLAW_STATE_DIR)
            let cron_data_dir = if let Some(state_dir) = std::env::var_os("OPENCLAW_STATE_DIR") {
                PathBuf::from(state_dir)
            } else {
                dirs_next::home_dir().unwrap_or_default().join(".openclaw")
            }.join("var").join("data");
            let runner = CronRunner::new(
                &cron_cfg,
                jobs,
                Arc::clone(&registry),
                Arc::clone(&channel_manager),
                cron_data_dir,
                cron_reload_tx.clone(),
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
    };
    crate::ws::tick::start_tick_loop(Arc::clone(&state.ws_conns));

    let bind_addr = resolve_bind_addr(&config);
    info!("starting HTTP server on {bind_addr}");

    // Background update check (non-blocking)
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let client = reqwest::Client::builder()
            .user_agent(concat!("rsclaw/", env!("RSCLAW_BUILD_VERSION")))
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
                let current_raw = env!("RSCLAW_BUILD_VERSION");
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

    // Clean up PID file on exit.
    let _ = std::fs::remove_file(&pid_file);
    result
}

// ---------------------------------------------------------------------------
// Provider construction
// ---------------------------------------------------------------------------

fn build_providers(config: &RuntimeConfig) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();

    if let Some(models_cfg) = &config.model.models {
        for (name, provider_cfg) in &models_cfg.providers {
            let api_key = provider_cfg
                .api_key
                .as_ref()
                .and_then(|k| k.as_plain().map(str::to_owned))
                .or_else(|| std::env::var(format!("{}_API_KEY", name.to_uppercase())).ok());

            let base_url = provider_cfg.base_url.clone().or_else(|| {
                // Fall back to well-known base URLs for named providers.
                match name.as_str() {
                    "qwen" => Some("https://dashscope.aliyuncs.com/compatible-mode/v1".to_owned()),
                    "deepseek" => Some("https://api.deepseek.com/v1".to_owned()),
                    "kimi" | "moonshot" => Some("https://api.moonshot.cn/v1".to_owned()),
                    "zhipu" => Some("https://open.bigmodel.cn/api/paas/v4".to_owned()),
                    "minimax" => Some("https://api.minimaxi.com/v1".to_owned()),
                    "siliconflow" => Some("https://api.siliconflow.cn/v1".to_owned()),
                    "groq"        => Some("https://api.groq.com/openai/v1".to_owned()),
                    "openrouter"  => Some("https://openrouter.ai/api/v1".to_owned()),
                    "gaterouter"  => Some("https://api.gaterouter.ai/openai/v1".to_owned()),
                    "grok" | "xai" => Some("https://api.x.ai/v1".to_owned()),
                    _ => None,
                }
            });

            // Resolve User-Agent: provider > gateway > built-in default.
            let user_agent = provider_cfg
                .user_agent
                .clone()
                .or_else(|| config.gateway.user_agent.clone());

            // Determine API format: explicit `api` field > name-based inference.
            let api_format = provider_cfg.api.clone().unwrap_or_else(|| {
                use crate::config::schema::ApiFormat;
                match name.as_str() {
                    "anthropic" => ApiFormat::Anthropic,
                    "gemini" => ApiFormat::Gemini,
                    "doubao" | "bytedance" => ApiFormat::OpenAiResponses,
                    "ollama" => ApiFormat::Ollama,
                    _ => ApiFormat::OpenAiCompletions,
                }
            });

            let provider: Arc<dyn LlmProvider> = match (name.as_str(), &api_format) {
                ("anthropic", _)
                | (_, &crate::config::schema::ApiFormat::Anthropic)
                | (_, &crate::config::schema::ApiFormat::AnthropicMessages) => {
                    let key = api_key
                        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                        .unwrap_or_default();
                    let url = base_url.unwrap_or_else(|| anthropic::ANTHROPIC_API_BASE.to_owned());
                    Arc::new(AnthropicProvider::with_user_agent(key, url, user_agent))
                }
                ("gemini", _) => {
                    let key = api_key
                        .or_else(|| std::env::var("GEMINI_API_KEY").ok())
                        .unwrap_or_default();
                    let url = base_url.unwrap_or_else(|| gemini::GEMINI_API_BASE.to_owned());
                    Arc::new(GeminiProvider::with_user_agent(key, url, user_agent))
                }
                (_, &crate::config::schema::ApiFormat::Ollama) => {
                    // Ollama backend: reasoning models use native /api/chat
                    let key = api_key.or_else(|| std::env::var("OPENAI_API_KEY").ok());
                    let url = base_url.unwrap_or_else(|| "http://localhost:11434".to_owned());
                    Arc::new(OpenAiProvider::ollama_with_ua(url, key, user_agent))
                }
                (_, &crate::config::schema::ApiFormat::OpenAiResponses) => {
                    let key = api_key.or_else(|| std::env::var("OPENAI_API_KEY").ok());
                    let url = base_url
                        .unwrap_or_else(|| crate::provider::openai::OPENAI_API_BASE.to_owned());
                    Arc::new(OpenAiProvider::responses_with_ua(url, key, user_agent))
                }
                _ => {
                    // OpenAI-compatible (covers openai-completions,
                    // llama.cpp, vLLM, SGLang, etc.)
                    let key = api_key.or_else(|| std::env::var("OPENAI_API_KEY").ok());
                    if let Some(url) = base_url {
                        Arc::new(OpenAiProvider::with_user_agent(url, key, user_agent))
                    } else {
                        Arc::new(OpenAiProvider::with_user_agent(
                            crate::provider::openai::OPENAI_API_BASE,
                            key,
                            user_agent,
                        ))
                    }
                }
            };

            tracing::info!(name=%name, api=?api_format, "provider registered");
            registry.register(name.clone(), provider);
        }
    }

    // Auto-register from environment variables.
    if !registry.names().contains(&"anthropic")
        && let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
    {
        registry.register("anthropic", Arc::new(AnthropicProvider::new(key)));
    }
    if !registry.names().contains(&"openai")
        && let Ok(key) = std::env::var("OPENAI_API_KEY")
    {
        registry.register("openai", Arc::new(OpenAiProvider::new(key)));
    }
    if !registry.names().contains(&"gemini")
        && let Ok(key) = std::env::var("GEMINI_API_KEY")
    {
        registry.register("gemini", Arc::new(GeminiProvider::new(key)));
    }

    // Auto-register OpenAI-compatible providers from env vars.
    let compat_providers = [
        // --- International ---
        ("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
        (
            "deepseek",
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
        ),
        ("mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
        (
            "together",
            "https://api.together.xyz/v1",
            "TOGETHER_API_KEY",
        ),
        (
            "openrouter",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
        ),
        ("xai", "https://api.x.ai/v1", "XAI_API_KEY"),
        ("cerebras", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY"),
        (
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
            "FIREWORKS_API_KEY",
        ),
        (
            "perplexity",
            "https://api.perplexity.ai",
            "PERPLEXITY_API_KEY",
        ),
        ("cohere", "https://api.cohere.com/v2", "COHERE_API_KEY"),
        (
            "huggingface",
            "https://api-inference.huggingface.co/v1",
            "HF_API_KEY",
        ),
        // --- China ---
        (
            "siliconflow",
            "https://api.siliconflow.cn/v1",
            "SILICONFLOW_API_KEY",
        ),
        (
            "qwen",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "DASHSCOPE_API_KEY",
        ),
        ("kimi", "https://api.moonshot.cn/v1", "MOONSHOT_API_KEY"), // Kimi = Moonshot
        ("moonshot", "https://api.moonshot.cn/v1", "MOONSHOT_API_KEY"),
        (
            "zhipu",
            "https://open.bigmodel.cn/api/paas/v4",
            "ZHIPU_API_KEY",
        ),
        (
            "baichuan",
            "https://api.baichuan-ai.com/v1",
            "BAICHUAN_API_KEY",
        ),
        ("minimax", "https://api.minimaxi.com/v1", "MINIMAX_API_KEY"),
        ("stepfun", "https://api.stepfun.com/v1", "STEPFUN_API_KEY"),
        ("lingyi", "https://api.lingyiwanwu.com/v1", "LINGYI_API_KEY"),
        (
            "baidu",
            "https://qianfan.baidubce.com/v2",
            "QIANFAN_API_KEY",
        ),
        (
            "gaterouter",
            "https://api.gaterouter.ai/openai/v1",
            "GATEROUTER_API_KEY",
        ),
        (
            "infini",
            "https://cloud.infini-ai.com/maas/v1",
            "INFINI_API_KEY",
        ),
    ];

    for (name, base_url, env_key) in compat_providers {
        if !registry.names().contains(&name)
            && let Ok(key) = std::env::var(env_key)
        {
            registry.register(
                name,
                Arc::new(OpenAiProvider::with_base_url(base_url, Some(key))),
            );
        }
    }

    // Doubao / ByteDance — uses OpenAI Responses API format.
    if !registry.names().contains(&"doubao") {
        if let Ok(key) = std::env::var("ARK_API_KEY").or_else(|_| std::env::var("DOUBAO_API_KEY")) {
            registry.register(
                "doubao",
                Arc::new(OpenAiProvider::responses(
                    "https://ark.cn-beijing.volces.com/api/v3",
                    Some(key),
                )),
            );
        }
    }
    if !registry.names().contains(&"bytedance") {
        if let Ok(key) = std::env::var("ARK_API_KEY").or_else(|_| std::env::var("DOUBAO_API_KEY")) {
            registry.register(
                "bytedance",
                Arc::new(OpenAiProvider::responses(
                    "https://ark.cn-beijing.volces.com/api/v3",
                    Some(key),
                )),
            );
        }
    }

    // Ollama (no API key needed).
    if !registry.names().contains(&"ollama") {
        registry.register(
            "ollama",
            Arc::new(OpenAiProvider::with_base_url(
                "http://localhost:11434",
                None,
            )),
        );
    }

    // Wire up model aliases from agents.defaults.models.
    if let Some(models) = &config.agents.defaults.models {
        let mut aliases = HashMap::new();
        for (model_key, alias_def) in models {
            if let Some(ref target) = alias_def.alias {
                aliases.insert(model_key.clone(), target.clone());
            } else if let Some(ref target_model) = alias_def.model {
                // model field: "deepseek/deepseek-v3" -> provider is first segment
                if let Some((prov, _)) = target_model.split_once('/') {
                    aliases.insert(model_key.clone(), prov.to_owned());
                }
            }
        }
        if !aliases.is_empty() {
            info!("{} model alias(es) configured", aliases.len());
            registry.set_model_aliases(aliases);
        }
    }

    registry
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

        tokio::spawn(async move {
            info!(agent_id = %handle.id, "agent runtime task started");
            while let Some(msg) = rx.recv().await {
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
                let reply = result.unwrap_or_else(|e| {
                    error!(agent = %handle.id, "turn error: {e:#}");
                    AgentReply {
                        text: format!("[error: {e}]"),
                        is_empty: false,
                        tool_calls: None,
                        images: vec![],
                        files: vec![],
                        pending_analysis: None,
                    }
                });
                let _ = reply_tx.send(reply);
            }
            info!(agent_id = %handle.id, "agent runtime task ended (channel closed)");
        });
    }
}

// ---------------------------------------------------------------------------
// Channel construction
// ---------------------------------------------------------------------------

fn default_dm_scope(config: &RuntimeConfig) -> DmScope {
    config
        .channel
        .session
        .dm_scope
        .clone()
        .unwrap_or(DmScope::PerChannelPeer)
}

/// Check if a message is a fast preparse command that should bypass the per-user queue.
/// These are local slash commands that execute instantly and should not wait behind
/// slow LLM requests in the queue.
fn is_fast_preparse(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_lowercase();
    // Single-word commands (no args needed)
    matches!(
        lower.as_str(),
        "/ls" | "/status" | "/version" | "/help" | "/?" | "/health" | "/uptime"
            | "/models" | "/clear" | "/abort" | "/sessions"
    )
    // Commands with optional/required args
    || lower.starts_with("/ls ")
    || lower.starts_with("/cat ")
    || lower.starts_with("/ss")
    || lower.starts_with("/remember ")
    || lower.starts_with("/recall ")
    || lower.starts_with("/model ")
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Processing indicator — send with 3s timeout to avoid blocking
// ---------------------------------------------------------------------------

/// Returns the configured processing timeout duration (default 60s).
/// When set to 0, returns a very large duration (effectively disabled).
fn processing_timeout(config: &RuntimeConfig) -> Duration {
    let secs = config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.processing_timeout)
        .unwrap_or(60);
    if secs == 0 {
        Duration::from_secs(86400)
    } else {
        Duration::from_secs(secs)
    }
}

async fn send_processing(
    tx: &mpsc::Sender<OutboundMessage>,
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
    let text = crate::i18n::t("processing", i18n_lang);
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        tx.send(OutboundMessage {
            target_id,
            is_group,
            text,
            reply_to: None,
            images: vec![],
            channel: None,

                    files: vec![],        }),
    )
    .await;
}

// ---------------------------------------------------------------------------
// /btw direct LLM call — bypasses agent inbox entirely
// ---------------------------------------------------------------------------

/// Perform a direct LLM call for /btw side queries, bypassing the agent inbox.
/// Reads the agent's live status so the LLM knows what the main agent is doing.
/// Returns the response text, or None on failure.
async fn btw_direct_call(
    question: &str,
    live_status: &tokio::sync::RwLock<LiveStatus>,
    providers: &ProviderRegistry,
    config: &RuntimeConfig,
) -> Option<String> {
    // 1. Read live status.
    let status_block = {
        let status = live_status.read().await;
        if status.state.is_empty() || status.state == "idle" {
            String::new()
        } else {
            let elapsed = status
                .started_at
                .map(|s| s.elapsed().as_secs())
                .unwrap_or(0);
            format!(
                "\n<main_agent_status>\nState: {}\nTask: {}\nElapsed: {}s\nRecent tools: {}\nResponse preview: {}\n</main_agent_status>",
                status.state,
                status.current_task,
                elapsed,
                status.tool_history.join(", "),
                status.text_preview,
            )
        }
    };

    // 2. Resolve model name.
    let model = config
        .agents
        .defaults
        .model
        .as_ref()
        .and_then(|m| m.primary.as_deref())
        .unwrap_or("anthropic/claude-sonnet-4-6");

    let system = format!(
        "You are answering a quick side question (/btw). Be concise and direct. \
         You have no tools available. Answer from your general knowledge only.{}",
        status_block
    );

    let req = LlmRequest {
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(question.to_owned()),
        }],
        tools: vec![],
        system: Some(system),
        max_tokens: Some(500),
        temperature: None,
        frequency_penalty: None,
        thinking_budget: None,
    };

    // 3. Create a simple failover manager (no fallbacks needed for /btw).
    let auth_order = config
        .model
        .auth
        .as_ref()
        .and_then(|a| a.order.clone())
        .unwrap_or_default();
    let mut failover = FailoverManager::new(auth_order, std::collections::HashMap::new(), vec![]);

    let mut stream = match failover.call(req, providers).await {
        Ok(s) => s,
        Err(e) => {
            warn!("/btw direct LLM call failed: {e:#}");
            return None;
        }
    };

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

    if text_buf.is_empty() {
        None
    } else {
        // Strip any residual <think>...</think> tags
        let cleaned = crate::provider::openai::strip_think_tags_pub(&text_buf);
        Some(cleaned)
    }
}

fn start_channels(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    feishu_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::feishu::FeishuChannel>>>,
    wecom_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::wecom::WeComChannel>>>,
    whatsapp_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::whatsapp::WhatsAppChannel>>>,
    line_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::line::LineChannel>>>,
    zalo_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::zalo::ZaloChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    // CLI channel — always started in local mode.
    {
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register CLI channel sender for notification routing.
        {
            let mut senders = channel_senders.write().unwrap();
            senders.insert("cli".to_string(), out_tx.clone());
        }

        let on_message = Arc::new(move |peer_id: String, text: String| {
            let reg = Arc::clone(&reg);
            let cfg = Arc::clone(&cfg_arc);
            let tx = out_tx.clone();
            tokio::spawn(async move {
                let handle = match reg.default_agent() {
                    Ok(h) => h,
                    Err(e) => {
                        error!("no default agent: {e:#}");
                        return;
                    }
                };
                let dm_scope = default_dm_scope(&cfg);
                let session_key = derive_session_key(&SessionKeyParams {
                    agent_id: handle.id.clone(),
                    kind: MessageKind::DirectMessage { account_id: None },
                    channel: "cli".to_string(),
                    peer_id: peer_id.clone(),
                    dm_scope,
                });
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let msg = AgentMessage {
                    session_key,
                    text,
                    channel: "cli".to_string(),
                    peer_id,
                    chat_id: String::new(),
                    reply_tx,
                    extra_tools: vec![],
                    images: vec![],
                    files: vec![],
                };
                if handle.tx.send(msg).await.is_err() {
                    return;
                }
                if let Ok(reply) = reply_rx.await {
                    let pending = reply.pending_analysis;
                    if !reply.is_empty {
                        let _ = tx
                            .send(OutboundMessage {
                                target_id: "local".to_string(),
                                is_group: false,
                                text: reply.text,
                                reply_to: None,
                                images: reply.images,
                                channel: None,
                                files: reply.files,
                            })
                            .await;
                    }
                    if let Some(analysis) = pending {
                        handle_pending_analysis(
                            analysis,
                            Arc::clone(&handle),
                            &tx,
                            "local".to_string(),
                            false,
                            &cfg,
                        )
                        .await;
                    }
                }
            });
        });

        let cli_ch = Arc::new(CliChannel::new(on_message));
        let cli_send = Arc::clone(&cli_ch);

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = cli_send.send(msg).await {
                    error!("CLI send error: {e:#}");
                }
            }
        });

        let _ = manager.register(Arc::clone(&cli_ch) as Arc<dyn Channel>);
        tokio::spawn(async move {
            if let Err(e) = cli_ch.run().await {
                error!("CLI channel error: {e:#}");
            }
        });
    }

    // Telegram — supports multiple accounts (OpenClaw format).
    if let Some(tg_cfg) = &config.channel.channels.telegram
        && tg_cfg.base.enabled.unwrap_or(true)
    {
        // Collect (account_name, bot_token) pairs.
        let mut tg_accounts: Vec<(String, String)> = Vec::new();

        // Legacy: single bot_token at top level.
        if let Some(token) = tg_cfg.bot_token.as_ref().and_then(|t| t.as_plain()) {
            tg_accounts.push(("default".to_owned(), token.to_owned()));
        }

        // OpenClaw: channels.telegram.accounts.<name>.botToken
        if let Some(accts) = &tg_cfg.accounts {
            for (name, acct) in accts {
                if let Some(t) = acct.get("botToken").and_then(|v| v.as_str()) {
                    // Avoid duplicate if top-level token == this account's token.
                    if !tg_accounts.iter().any(|(_, existing)| existing == t) {
                        tg_accounts.push((name.clone(), t.to_owned()));
                    }
                }
            }
        }

        if tg_accounts.is_empty() {
            warn!("telegram bot_token not set, channel disabled");
        }

        // Load dmPolicy and groupPolicy from config.
        let dm_policy = tg_cfg
            .base
            .dm_policy
            .clone()
            .unwrap_or(crate::config::schema::DmPolicy::Pairing);
        let group_policy = tg_cfg
            .base
            .group_policy
            .clone()
            .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
        let group_allow_from: Vec<String> =
            tg_cfg.base.group_allow_from.clone().unwrap_or_default();
        let allow_from: Vec<String> = tg_cfg.base.allow_from.clone().unwrap_or_default();

        let enforcer = Arc::new(
            crate::channel::DmPolicyEnforcer::new(dm_policy.clone(), allow_from)
                .with_persistence("telegram", Arc::clone(&redb_store)),
        );

        // Register enforcer so the pairing API can approve codes.
        if let Ok(mut enforcers) = dm_enforcers.write() {
            enforcers.insert("telegram".to_owned(), Arc::clone(&enforcer));
        }

        for (acct_name, token) in tg_accounts {
            // Find binding for this account to determine which agent handles it.
            let bound_agent = config
                .agents
                .bindings
                .iter()
                .find(|b| {
                    b.match_.channel.as_deref() == Some("telegram")
                        && b.match_.account_id.as_deref() == Some(&acct_name)
                })
                .map(|b| b.agent_id.clone());

            let reg = Arc::clone(&registry);
            let cfg_arc = Arc::new(config.clone());
            let acct_for_log = acct_name.clone();
            let bound = bound_agent.clone();
            let enforcer = Arc::clone(&enforcer);
            let gp = Arc::new(group_policy.clone());
            let ga = Arc::new(group_allow_from.clone());
            let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

            // Register Telegram channel sender for notification routing.
            {
                let mut senders = channel_senders.write().unwrap();
                senders.insert(format!("telegram/{}", acct_name), out_tx.clone());
            }

            // Per-user inbound queue: serializes messages so each user's messages
            // are processed one at a time, preventing reply channel drops.
            type TgItem = (
                String,
                i64,
                i64,
                bool,
                Option<String>,
                Vec<crate::agent::registry::ImageAttachment>,
                Vec<crate::agent::registry::FileAttachment>,
            );
            let tg_user_queues: Arc<
                tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<TgItem>>>,
            > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

            let on_message = Arc::new(
                move |peer_id: i64,
                      text: String,
                      chat_id: i64,
                      is_group: bool,
                      _thread: Option<i64>,
                      images: Vec<crate::agent::registry::ImageAttachment>,
                      file_attachments: Vec<crate::agent::registry::FileAttachment>| {
                    let reg = Arc::clone(&reg);
                    let cfg = Arc::clone(&cfg_arc);
                    let tx = out_tx.clone();
                    let bound = bound.clone();
                    let enforcer = Arc::clone(&enforcer);
                    let group_policy = Arc::clone(&gp);
                    let group_allow = Arc::clone(&ga);
                    let queues = Arc::clone(&tg_user_queues);
                    tokio::spawn(async move {
                        // Group policy check.
                        if is_group {
                            match group_policy.as_ref() {
                                crate::config::schema::GroupPolicy::Disabled => {
                                    debug!(chat_id, "telegram group message rejected: groupPolicy=disabled");
                                    return;
                                }
                                crate::config::schema::GroupPolicy::Allowlist => {
                                    let cid = chat_id.to_string();
                                    if !group_allow.iter().any(|g| *g == cid) {
                                        debug!(chat_id, "telegram group message rejected: not in groupAllowFrom");
                                        return;
                                    }
                                }
                                crate::config::schema::GroupPolicy::Open => {}
                            }
                        }
                        // DM policy check.
                        if !is_group {
                            use crate::channel::PolicyResult;
                            match enforcer.check(&peer_id.to_string()).await {
                                PolicyResult::Allow => {}
                                PolicyResult::Deny => {
                                    debug!(peer_id, "telegram DM rejected by policy");
                                    return;
                                }
                                PolicyResult::SendPairingCode(code) => {
                                    let _ = tx
                                        .send(OutboundMessage {
                                            target_id: chat_id.to_string(),
                                            is_group: false,
                                            text: crate::i18n::t_fmt("pairing_required", crate::i18n::default_lang(), &[("code", &code)]),
                                            reply_to: None,
                                            images: vec![],
            channel: None,

                    files: vec![],                                        })
                                        .await;
                                    return;
                                }
                                PolicyResult::PairingQueueFull => {
                                    let _ = tx
                                        .send(OutboundMessage {
                                            target_id: chat_id.to_string(),
                                            is_group: false,
                                            text: crate::i18n::t("pairing_queue_full", crate::i18n::default_lang()).to_owned(),
                                            reply_to: None,
                                            images: vec![],
            channel: None,

                    files: vec![],                                        })
                                        .await;
                                    return;
                                }
                            }
                        }

                        // Get or create a per-user queue.
                        let queue_key = peer_id.to_string();
                        let user_tx = {
                            let mut map = queues.lock().await;
                            let needs_create = match map.get(&queue_key) {
                                Some(existing) if !existing.is_closed() => false,
                                Some(_) => { map.remove(&queue_key); true }
                                None => true,
                            };
                            if needs_create {
                                let (utx, mut urx) = mpsc::channel::<TgItem>(32);
                                map.insert(queue_key.clone(), utx.clone());
                                let w_reg = Arc::clone(&reg);
                                let w_cfg = Arc::clone(&cfg);
                                let w_tx = tx.clone();
                                let w_uid = queue_key.clone();
                                tokio::spawn(async move {
                                    while let Some((text, peer_id, chat_id, is_group, bound, images, file_attachments)) = urx.recv().await {
                                        let process_result = tokio::time::timeout(
                                            Duration::from_secs(600),
                                            async {
                                        let handle = if let Some(ref agent_id) = bound {
                                            match w_reg.get(agent_id) {
                                                Ok(h) => h,
                                                Err(_) => match w_reg.route("telegram") {
                                                    Ok(h) => h,
                                                    Err(e) => { error!("route error: {e:#}"); return; }
                                                },
                                            }
                                        } else {
                                            match w_reg.route("telegram") {
                                                Ok(h) => h,
                                                Err(e) => { error!("route error: {e:#}"); return; }
                                            }
                                        };
                                        let dm_scope = default_dm_scope(&w_cfg);
                                        let session_key = derive_session_key(&SessionKeyParams {
                                            agent_id: handle.id.clone(),
                                            kind: if is_group {
                                                MessageKind::GroupMessage {
                                                    group_id: chat_id.to_string(),
                                                    thread_id: None,
                                                }
                                            } else {
                                                MessageKind::DirectMessage { account_id: None }
                                            },
                                            channel: "telegram".to_string(),
                                            peer_id: peer_id.to_string(),
                                            dm_scope,
                                        });
                                        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                        let msg = AgentMessage {
                                            session_key,
                                            text,
                                            channel: "telegram".to_string(),
                                            peer_id: peer_id.to_string(),
                                            chat_id: String::new(),
                                            reply_tx,
                                            extra_tools: vec![],
                                            images,
                                            files: file_attachments,
                                        };
                                        if handle.tx.send(msg).await.is_err() {
                                            return;
                                        }
                                        let reply = tokio::select! {
                                            result = &mut reply_rx => result,
                                            _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                                send_processing(&w_tx, chat_id.to_string(), is_group, &w_cfg).await;
                                                reply_rx.await
                                            }
                                        };
                                        if let Ok(r) = reply {
                                            let pending = r.pending_analysis;
                                            if !r.is_empty {
                                                let _ = w_tx
                                                    .send(OutboundMessage {
                                                        target_id: chat_id.to_string(),
                                                        is_group,
                                                        text: r.text,
                                                        reply_to: None,
                                                        images: r.images,
                                                        files: r.files,
                                                        channel: None,                                                    })
                                                    .await;
                                            }
                                            if let Some(analysis) = pending {
                                                handle_pending_analysis(
                                                    analysis, Arc::clone(&handle), &w_tx,
                                                    chat_id.to_string(), is_group, &w_cfg,
                                                ).await;
                                            }
                                        }
                                            }
                                        ).await;
                                        if process_result.is_err() {
                                            warn!(user = %w_uid, "telegram: message processing timed out (600s), skipping to next");
                                        }
                                    }
                                    debug!(user = %w_uid, "telegram: per-user worker stopped");
                                });
                                utx
                            } else {
                                map.get(&queue_key).unwrap().clone()
                            }
                        };
                        // /btw bypass: spawn directly, skip the per-user queue
                        if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                            let reg = Arc::clone(&reg);
                            let tx = tx.clone();
                            let cfg = Arc::clone(&cfg);
                            let question = text[5..].to_owned();
                            let chat_id_s = chat_id.to_string();
                            let bound = bound.clone();
                            tokio::spawn(async move {
                                let handle = if let Some(ref agent_id) = bound {
                                    match reg.get(agent_id) {
                                        Ok(h) => h,
                                        Err(_) => match reg.route("telegram") {
                                            Ok(h) => h,
                                            Err(_) => return,
                                        },
                                    }
                                } else {
                                    match reg.route("telegram") {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    }
                                };
                                if let Some(reply_text) = btw_direct_call(
                                    &question, &handle.live_status, &handle.providers, &cfg,
                                ).await {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: chat_id_s,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
            channel: None,

                    files: vec![],                                    }).await;
                                }
                            });
                            return;
                        }
                        // Fast preparse bypass: local commands skip per-user queue
                        if is_fast_preparse(&text) {
                            let reg = Arc::clone(&reg);
                            let tx = tx.clone();
                            let cfg = Arc::clone(&cfg);
                            let peer_id_s = peer_id.to_string();
                            let chat_id_s = chat_id.to_string();
                            let bound = bound.clone();
                            tokio::spawn(async move {
                                let handle = if let Some(ref agent_id) = bound {
                                    match reg.get(agent_id) {
                                        Ok(h) => h,
                                        Err(_) => match reg.route("telegram") {
                                            Ok(h) => h,
                                            Err(_) => return,
                                        },
                                    }
                                } else {
                                    match reg.route("telegram") {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    }
                                };
                                let dm_scope = default_dm_scope(&cfg);
                                let session_key = derive_session_key(&SessionKeyParams {
                                    agent_id: handle.id.clone(),
                                    kind: if is_group {
                                        MessageKind::GroupMessage {
                                            group_id: chat_id_s.clone(),
                                            thread_id: None,
                                        }
                                    } else {
                                        MessageKind::DirectMessage { account_id: None }
                                    },
                                    channel: "telegram".to_string(),
                                    peer_id: peer_id_s.clone(),
                                    dm_scope,
                                });
                                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                                let msg = AgentMessage {
                                    session_key,
                                    text,
                                    channel: "telegram".to_string(),
                                    peer_id: peer_id_s,
                                    chat_id: String::new(),
                                    reply_tx,
                                    extra_tools: vec![],
                                    images,
                                    files: file_attachments,
                                };
                                if handle.tx.send(msg).await.is_err() {
                                    return;
                                }
                                if let Ok(r) = reply_rx.await {
                                    if !r.is_empty {
                                        let _ = tx.send(OutboundMessage {
                                            target_id: chat_id_s,
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,
                                        }).await;
                                    }
                                }
                            });
                            return;
                        }
                        if let Err(e) = user_tx.try_send((text, peer_id, chat_id, is_group, bound, images, file_attachments)) {
                            warn!(user = %queue_key, error = %e, "telegram: user queue full, dropping message");
                        }
                    });
                },
            );

            let api_base = tg_cfg.api_base.clone();
            let tg = Arc::new(TelegramChannel::new(token, api_base, on_message));
            let tg_send = Arc::clone(&tg);

            tokio::spawn(async move {
                while let Some(msg) = out_rx.recv().await {
                    if let Err(e) = tg_send.send(msg).await {
                        error!("telegram send error: {e:#}");
                    }
                }
            });

            let _ = manager.register(Arc::clone(&tg) as Arc<dyn Channel>);
            tokio::spawn(async move {
                if let Err(e) = tg.run().await {
                    error!("telegram channel error: {e:#}");
                }
            });
            info!(account = %acct_for_log, "telegram channel started");
        }
    }

    start_discord_if_configured(
        config,
        registry.clone(),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_slack_if_configured(
        config,
        registry.clone(),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_whatsapp_if_configured(
        config,
        registry.clone(),
        manager,
        whatsapp_slot,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_line_if_configured(
        config,
        registry.clone(),
        manager,
        line_slot,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_zalo_if_configured(
        config,
        registry.clone(),
        manager,
        zalo_slot,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_signal_if_configured(
        config,
        registry.clone(),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_wechat_personal_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_feishu_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&feishu_slot),
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_dingtalk_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_qq_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_matrix_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
    start_wecom_if_configured(
        config,
        Arc::clone(&registry),
        manager,
        wecom_slot,
        Arc::clone(&dm_enforcers),
        Arc::clone(&redb_store),
        Arc::clone(&channel_senders),
    );
}

fn start_discord_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    use crate::channel::discord::DiscordChannel;

    let Some(dc_cfg) = &config.channel.channels.discord else {
        return;
    };
    if !dc_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Collect (account_name, token) pairs.
    let mut dc_accounts: Vec<(String, String)> = Vec::new();

    // Legacy: single token at top level.
    if let Some(token) = dc_cfg.token.as_ref().and_then(|t| t.as_plain()) {
        dc_accounts.push(("default".to_owned(), token.to_owned()));
    }

    // Multi-account: channels.discord.accounts.<name>.token
    if let Some(accts) = &dc_cfg.accounts {
        for (name, acct) in accts {
            if let Some(t) = acct.get("token").and_then(|v| v.as_str()) {
                if !dc_accounts.iter().any(|(_, existing)| existing == t) {
                    dc_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }
    }

    if dc_accounts.is_empty() {
        warn!("discord token not set");
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = dc_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = dc_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = dc_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = dc_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("discord", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("discord".to_owned(), Arc::clone(&enforcer));
    }

    let allow_bots = dc_cfg.allow_bots.unwrap_or(false);

    for (acct_name, token) in dc_accounts {
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register Discord channel sender for notification routing.
        {
            let mut senders = channel_senders.write().unwrap();
            senders.insert(format!("discord/{}", acct_name), out_tx.clone());
        }

        // Find binding for this account.
        let bound_agent = config
            .agents
            .bindings
            .iter()
            .find(|b| {
                b.match_.channel.as_deref() == Some("discord")
                    && b.match_.account_id.as_deref() == Some(&acct_name)
            })
            .map(|b| b.agent_id.clone());
        let bound = bound_agent.clone();

        // Per-user inbound queue for Discord.
        type DcItem = (String, String, String, bool, Option<String>);
        let dc_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<DcItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |peer_id: String, text: String, channel_id: String, is_guild: bool| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let bound = bound.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&dc_user_queues);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_guild {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("discord group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == channel_id) {
                                    debug!("discord group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_guild {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&peer_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %peer_id, "discord DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: channel_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: channel_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&peer_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&peer_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<DcItem>(32);
                            map.insert(peer_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_tx = tx.clone();
                            let w_uid = peer_id.clone();
                            tokio::spawn(async move {
                                while let Some((text, peer_id, channel_id, is_guild, bound)) =
                                    urx.recv().await
                                {
                                    let process_result = tokio::time::timeout(
                                        Duration::from_secs(172800), // 48 hours, matching OpenClaw default
                                        async {
                                    let handle = if let Some(ref agent_id) = bound {
                                        match w_reg.get(agent_id) {
                                            Ok(h) => h,
                                            Err(_) => match w_reg.route("discord") {
                                                Ok(h) => h,
                                                Err(e) => { error!("discord route: {e:#}"); return; }
                                            },
                                        }
                                    } else {
                                        match w_reg.route("discord") {
                                            Ok(h) => h,
                                            Err(e) => { error!("discord route: {e:#}"); return; }
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: if is_guild {
                                            MessageKind::GroupMessage {
                                                group_id: channel_id.clone(),
                                                thread_id: None,
                                            }
                                        } else {
                                            MessageKind::DirectMessage { account_id: None }
                                        },
                                        channel: "discord".to_string(),
                                        peer_id: peer_id.clone(),
                                        dm_scope,
                                    });
                                    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                    let msg = AgentMessage {
                                        session_key,
                                        text,
                                        channel: "discord".to_string(),
                                        peer_id,
                                        chat_id: String::new(),
                                        reply_tx,
                                        extra_tools: vec![],
                                        images: vec![],
                                        files: vec![],
                                    };
                                    if handle.tx.send(msg).await.is_err() {
                                        return;
                                    }
                                    let reply = tokio::select! {
                                        result = &mut reply_rx => result,
                                        _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                            send_processing(&w_tx, channel_id.clone(), is_guild, &w_cfg).await;
                                            reply_rx.await
                                        }
                                    };
                                    if let Ok(r) = reply {
                                        let pending = r.pending_analysis;
                                        if !r.is_empty {
                                            let _ = w_tx
                                                .send(OutboundMessage {
                                                    target_id: channel_id.clone(),
                                                    is_group: is_guild,
                                                    text: r.text,
                                                    reply_to: None,
                                                    images: r.images,
                                                    files: r.files,
                                                    channel: None,                                                })
                                                .await;
                                        }
                                        if let Some(analysis) = pending {
                                            handle_pending_analysis(
                                                analysis, Arc::clone(&handle), &w_tx,
                                                channel_id, is_guild, &w_cfg,
                                            ).await;
                                        }
                                    }
                                        }
                                    ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "discord: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "discord: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&peer_id).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let channel_id = channel_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("discord") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: channel_id,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let peer_id = peer_id.clone();
                        let channel_id = channel_id.clone();
                        let bound = bound.clone();
                        tokio::spawn(async move {
                            let handle = if let Some(ref agent_id) = bound {
                                match reg.get(agent_id) {
                                    Ok(h) => h,
                                    Err(_) => match reg.route("discord") {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    },
                                }
                            } else {
                                match reg.route("discord") {
                                    Ok(h) => h,
                                    Err(_) => return,
                                }
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_guild {
                                    MessageKind::GroupMessage {
                                        group_id: channel_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "discord".to_string(),
                                peer_id: peer_id.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "discord".to_string(),
                                peer_id,
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images: vec![],
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: channel_id,
                                        is_group: is_guild,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) =
                        user_tx.try_send((text, peer_id.clone(), channel_id, is_guild, bound))
                    {
                        warn!(user = %peer_id, error = %e, "discord: user queue full, dropping message");
                    }
                });
            },
        );

        let dc = Arc::new(DiscordChannel::new(
            token,
            allow_bots,
            on_message,
            dc_cfg.api_base.clone(),
            dc_cfg.gateway_url.clone(),
        ));
        let dc_send = Arc::clone(&dc);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = dc_send.send(msg).await {
                    error!("discord send: {e:#}");
                }
            }
        });
        let _ = manager.register(Arc::clone(&dc) as Arc<dyn Channel>);
        tokio::spawn(async move {
            if let Err(e) = dc.run().await {
                error!("discord channel: {e:#}");
            }
        });
        info!(account = %acct_for_log, "discord channel started");
    }
}

fn start_slack_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    use crate::channel::slack::SlackChannel;

    let Some(sl_cfg) = &config.channel.channels.slack else {
        return;
    };
    if !sl_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Collect (account_name, bot_token, app_token, api_base) tuples.
    let mut sl_accounts: Vec<(String, String, Option<String>, Option<String>)> = Vec::new();

    // Legacy: single bot_token at top level.
    if let Some(bot_token) = sl_cfg.bot_token.as_ref().and_then(|t| t.as_plain()) {
        let app_token = sl_cfg
            .app_token
            .as_ref()
            .and_then(|t| t.as_plain())
            .map(str::to_owned);
        sl_accounts.push((
            "default".to_owned(),
            bot_token.to_owned(),
            app_token,
            sl_cfg.api_base.clone(),
        ));
    }

    // Multi-account: channels.slack.accounts.<name>.{botToken, appToken?, apiBase?}
    if let Some(accts) = &sl_cfg.accounts {
        for (name, acct) in accts {
            if let Some(bt) = acct.get("botToken").and_then(|v| v.as_str()) {
                if !sl_accounts.iter().any(|(_, existing, _, _)| existing == bt) {
                    let at = acct
                        .get("appToken")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    let ab = acct
                        .get("apiBase")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    sl_accounts.push((
                        name.clone(),
                        bt.to_owned(),
                        at,
                        ab.or_else(|| sl_cfg.api_base.clone()),
                    ));
                }
            }
        }
    }

    if sl_accounts.is_empty() {
        warn!("slack bot_token not set");
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = sl_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = sl_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = sl_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = sl_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("slack", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("slack".to_owned(), Arc::clone(&enforcer));
    }

    for (acct_name, bot_token, app_token, api_base) in sl_accounts {
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register Slack channel sender for notification routing.
        {
            let mut senders = channel_senders.write().unwrap();
            senders.insert(format!("slack/{}", acct_name), out_tx.clone());
        }

        // Find binding for this account.
        let bound_agent = config
            .agents
            .bindings
            .iter()
            .find(|b| {
                b.match_.channel.as_deref() == Some("slack")
                    && b.match_.account_id.as_deref() == Some(&acct_name)
            })
            .map(|b| b.agent_id.clone());
        let bound = bound_agent.clone();

        // Per-user inbound queue for Slack.
        type SlItem = (String, String, String, bool, Option<String>);
        let sl_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<SlItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |peer_id: String, text: String, channel_id: String, is_channel: bool| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let bound = bound.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&sl_user_queues);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_channel {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("slack group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == channel_id) {
                                    debug!("slack group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_channel {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&peer_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %peer_id, "slack DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: channel_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: channel_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&peer_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&peer_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<SlItem>(32);
                            map.insert(peer_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_tx = tx.clone();
                            let w_uid = peer_id.clone();
                            tokio::spawn(async move {
                                while let Some((text, peer_id, channel_id, is_channel, bound)) =
                                    urx.recv().await
                                {
                                    let process_result = tokio::time::timeout(
                                        Duration::from_secs(172800), // 48 hours, matching OpenClaw default
                                        async {
                                    let handle = if let Some(ref agent_id) = bound {
                                        match w_reg.get(agent_id) {
                                            Ok(h) => h,
                                            Err(_) => match w_reg.route("slack") {
                                                Ok(h) => h,
                                                Err(e) => { error!("slack route: {e:#}"); return; }
                                            },
                                        }
                                    } else {
                                        match w_reg.route("slack") {
                                            Ok(h) => h,
                                            Err(e) => { error!("slack route: {e:#}"); return; }
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: if is_channel {
                                            MessageKind::GroupMessage {
                                                group_id: channel_id.clone(),
                                                thread_id: None,
                                            }
                                        } else {
                                            MessageKind::DirectMessage { account_id: None }
                                        },
                                        channel: "slack".to_string(),
                                        peer_id: peer_id.clone(),
                                        dm_scope,
                                    });
                                    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                    let msg = AgentMessage {
                                        session_key,
                                        text,
                                        channel: "slack".to_string(),
                                        peer_id,
                                        chat_id: String::new(),
                                        reply_tx,
                                        extra_tools: vec![],
                                        images: vec![],
                                        files: vec![],
                                    };
                                    if handle.tx.send(msg).await.is_err() {
                                        return;
                                    }
                                    let reply = tokio::select! {
                                        result = &mut reply_rx => result,
                                        _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                            send_processing(&w_tx, channel_id.clone(), is_channel, &w_cfg).await;
                                            reply_rx.await
                                        }
                                    };
                                    if let Ok(r) = reply {
                                        let pending = r.pending_analysis;
                                        if !r.is_empty {
                                            let _ = w_tx
                                                .send(OutboundMessage {
                                                    target_id: channel_id.clone(),
                                                    is_group: is_channel,
                                                    text: r.text,
                                                    reply_to: None,
                                                    images: r.images,
                                                    files: r.files,
                                                    channel: None,                                                })
                                                .await;
                                        }
                                        if let Some(analysis) = pending {
                                            handle_pending_analysis(
                                                analysis, Arc::clone(&handle), &w_tx,
                                                channel_id, is_channel, &w_cfg,
                                            ).await;
                                        }
                                    }
                                        }
                                    ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "slack: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "slack: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&peer_id).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let channel_id = channel_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("slack") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: channel_id,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let peer_id = peer_id.clone();
                        let channel_id = channel_id.clone();
                        let bound = bound.clone();
                        tokio::spawn(async move {
                            let handle = if let Some(ref agent_id) = bound {
                                match reg.get(agent_id) {
                                    Ok(h) => h,
                                    Err(_) => match reg.route("slack") {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    },
                                }
                            } else {
                                match reg.route("slack") {
                                    Ok(h) => h,
                                    Err(_) => return,
                                }
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_channel {
                                    MessageKind::GroupMessage {
                                        group_id: channel_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "slack".to_string(),
                                peer_id: peer_id.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "slack".to_string(),
                                peer_id,
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images: vec![],
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: channel_id,
                                        is_group: is_channel,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) =
                        user_tx.try_send((text, peer_id.clone(), channel_id, is_channel, bound))
                    {
                        warn!(user = %peer_id, error = %e, "slack: user queue full, dropping message");
                    }
                });
            },
        );

        let sl = Arc::new(SlackChannel::new(
            bot_token, app_token, api_base, on_message,
        ));
        let sl_send = Arc::clone(&sl);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = sl_send.send(msg).await {
                    error!("slack send: {e:#}");
                }
            }
        });
        let _ = manager.register(Arc::clone(&sl) as Arc<dyn Channel>);
        tokio::spawn(async move {
            if let Err(e) = sl.run().await {
                error!("slack channel: {e:#}");
            }
        });
        info!(account = %acct_for_log, "slack channel started");
    }
}

fn start_whatsapp_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    whatsapp_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::whatsapp::WhatsAppChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    use crate::channel::whatsapp::WhatsAppChannel;

    let Some(wa_cfg) = &config.channel.channels.whatsapp else {
        return;
    };
    if !wa_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy from config (WhatsApp is DM-only, no group policy needed).
    let dm_policy = wa_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = wa_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("whatsapp", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("whatsapp".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, phone_number_id, access_token) tuples.
    let mut wa_accounts: Vec<(String, String, String)> = Vec::new();

    // Legacy: credentials from env vars.
    if let (Ok(pid), Ok(token)) = (
        std::env::var("WHATSAPP_PHONE_NUMBER_ID"),
        std::env::var("WHATSAPP_ACCESS_TOKEN"),
    ) {
        wa_accounts.push(("default".to_owned(), pid, token));
    }

    // Multi-account: channels.whatsapp.accounts.<name>.{phoneNumberId, accessToken}
    if let Some(accts) = &wa_cfg.accounts {
        for (name, acct) in accts {
            let pid = acct
                .get("phoneNumberId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let token = acct
                .get("accessToken")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !pid.is_empty() && !token.is_empty() {
                if !wa_accounts.iter().any(|(_, epid, _)| epid == pid) {
                    wa_accounts.push((name.clone(), pid.to_owned(), token.to_owned()));
                }
            }
        }
    }

    if wa_accounts.is_empty() {
        warn!("WHATSAPP_PHONE_NUMBER_ID not set, whatsapp disabled");
        return;
    }

    for (acct_name, phone_number_id, access_token) in wa_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register WhatsApp channel sender for notification routing.
        {
            let mut senders = channel_senders.write().unwrap();
            senders.insert(format!("whatsapp/{}", acct_name), out_tx.clone());
        }

        // Per-user inbound queue for WhatsApp.
        type WaItem = (String, String, Vec<crate::agent::registry::ImageAttachment>);
        let wa_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<WaItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |from: String,
                  text: String,
                  images: Vec<crate::agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let queues = Arc::clone(&wa_user_queues);
                tokio::spawn(async move {
                    // DM policy check (WhatsApp is DM-only).
                    {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&from).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %from, "whatsapp DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: from.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: from.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&from) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&from);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<WaItem>(32);
                            map.insert(from.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_tx = tx.clone();
                            let w_uid = from.clone();
                            tokio::spawn(async move {
                                while let Some((text, from, images)) = urx.recv().await {
                                    let process_result = tokio::time::timeout(
                                Duration::from_secs(600),
                                async {
                            let handle = match w_reg.route("whatsapp") {
                                Ok(h) => h,
                                Err(e) => { error!("whatsapp route: {e:#}"); return; }
                            };
                            let dm_scope = default_dm_scope(&w_cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: MessageKind::DirectMessage { account_id: None },
                                channel: "whatsapp".to_string(),
                                peer_id: from.clone(),
                                dm_scope,
                            });
                            let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "whatsapp".to_string(),
                                peer_id: from.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            let reply = tokio::select! {
                                result = &mut reply_rx => result,
                                _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                    send_processing(&w_tx, from.clone(), false, &w_cfg).await;
                                    reply_rx.await
                                }
                            };
                            if let Ok(r) = reply {
                                let pending = r.pending_analysis;
                                if !r.is_empty {
                                    let _ = w_tx
                                        .send(OutboundMessage {
                                            target_id: from.clone(),
                                            is_group: false,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,                                        })
                                        .await;
                                }
                                if let Some(analysis) = pending {
                                    handle_pending_analysis(
                                        analysis, Arc::clone(&handle), &w_tx,
                                        from, false, &w_cfg,
                                    ).await;
                                }
                            }
                                }
                            ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "whatsapp: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "whatsapp: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&from).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let from = from.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("whatsapp") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: from,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let from = from.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("whatsapp") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: MessageKind::DirectMessage { account_id: None },
                                channel: "whatsapp".to_string(),
                                peer_id: from.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "whatsapp".to_string(),
                                peer_id: from.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: from,
                                        is_group: false,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) = user_tx.try_send((text, from.clone(), images)) {
                        warn!(user = %from, error = %e, "whatsapp: user queue full, dropping message");
                    }
                });
            },
        );

        let wa = Arc::new(WhatsAppChannel::with_api_base(
            phone_number_id,
            access_token,
            wa_cfg.api_base.clone(),
            on_message,
        ));
        let _ = whatsapp_slot.set(Arc::clone(&wa));
        let wa_send = Arc::clone(&wa);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = wa_send.send(msg).await {
                    error!("whatsapp send: {e:#}");
                }
            }
        });
        let _ = manager.register(Arc::clone(&wa) as Arc<dyn Channel>);
        tokio::spawn(async move {
            if let Err(e) = wa.run().await {
                error!("whatsapp channel: {e:#}");
            }
        });
        info!(account = %acct_for_log, "whatsapp channel started (webhook mode)");
    } // end for wa_accounts
}

fn start_line_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    line_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::line::LineChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    use crate::channel::line::LineChannel;

    let Some(line_cfg) = &config.channel.channels.line else {
        return;
    };
    if !line_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = line_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = line_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = line_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = line_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("line", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("line".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, access_token) pairs.
    let mut line_accounts: Vec<(String, String)> = Vec::new();

    // Legacy: single token at top level.
    if let Some(token) = line_cfg
        .channel_access_token
        .as_ref()
        .and_then(|t| t.as_plain())
        .map(str::to_owned)
        .or_else(|| std::env::var("LINE_CHANNEL_ACCESS_TOKEN").ok())
    {
        line_accounts.push(("default".to_owned(), token));
    }

    // Multi-account: channels.line.accounts.<name>.channelAccessToken
    if let Some(accts) = &line_cfg.accounts {
        for (name, acct) in accts {
            if let Some(t) = acct.get("channelAccessToken").and_then(|v| v.as_str()) {
                if !line_accounts.iter().any(|(_, et)| et == t) {
                    line_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }
    }

    if line_accounts.is_empty() {
        warn!("LINE channel_access_token not set, line disabled");
        return;
    }

    for (acct_name, channel_access_token) in line_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for LINE.
        type LineItem = (
            String,
            String,
            bool,
            Vec<crate::agent::registry::ImageAttachment>,
        );
        let line_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<LineItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |user_id: String,
                  text: String,
                  is_group: bool,
                  images: Vec<crate::agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&line_user_queues);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("line group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == user_id) {
                                    debug!("line group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&user_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %user_id, "line DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: user_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: user_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&user_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&user_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<LineItem>(32);
                            map.insert(user_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_tx = tx.clone();
                            let w_uid = user_id.clone();
                            tokio::spawn(async move {
                                while let Some((text, user_id, is_group, images)) = urx.recv().await
                                {
                                    let process_result = tokio::time::timeout(
                                    Duration::from_secs(600),
                                    async {
                                let handle = match w_reg.route("line") {
                                    Ok(h) => h,
                                    Err(e) => { error!("line route: {e:#}"); return; }
                                };
                                let dm_scope = default_dm_scope(&w_cfg);
                                let session_key = derive_session_key(&SessionKeyParams {
                                    agent_id: handle.id.clone(),
                                    kind: if is_group {
                                        MessageKind::DirectMessage { account_id: None }
                                    } else {
                                        MessageKind::DirectMessage { account_id: None }
                                    },
                                    channel: "line".to_string(),
                                    peer_id: user_id.clone(),
                                    dm_scope,
                                });
                                let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                let msg = AgentMessage {
                                    session_key,
                                    text,
                                    channel: "line".to_string(),
                                    peer_id: user_id.clone(),
                                    chat_id: String::new(),
                                    reply_tx,
                                    extra_tools: vec![],
                                    images,
                                    files: vec![],
                                };
                                if handle.tx.send(msg).await.is_err() {
                                    return;
                                }
                                let reply = tokio::select! {
                                    result = &mut reply_rx => result,
                                    _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                        send_processing(&w_tx, user_id.clone(), is_group, &w_cfg).await;
                                        reply_rx.await
                                    }
                                };
                                if let Ok(r) = reply {
                                    let pending = r.pending_analysis;
                                    if !r.is_empty {
                                        let _ = w_tx
                                            .send(OutboundMessage {
                                                target_id: user_id.clone(),
                                                is_group,
                                                text: r.text,
                                                reply_to: None,
                                                images: r.images,
                                                files: r.files,
                                                channel: None,                                            })
                                            .await;
                                    }
                                    if let Some(analysis) = pending {
                                        handle_pending_analysis(
                                            analysis, Arc::clone(&handle), &w_tx,
                                            user_id, is_group, &w_cfg,
                                        ).await;
                                    }
                                }
                                    }
                                ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "line: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "line: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&user_id).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let user_id = user_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("line") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: user_id,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let user_id = user_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("line") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: user_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "line".to_string(),
                                peer_id: user_id.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "line".to_string(),
                                peer_id: user_id.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: user_id,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) = user_tx.try_send((text, user_id.clone(), is_group, images)) {
                        warn!(user = %user_id, error = %e, "line: user queue full, dropping message");
                    }
                });
            },
        );

        let line = Arc::new(LineChannel::with_api_base(
            channel_access_token,
            line_cfg.api_base.clone(),
            on_message,
        ));
        let _ = line_slot.set(Arc::clone(&line));
        let line_send = Arc::clone(&line);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = line_send.send(msg).await {
                    error!("line send: {e:#}");
                }
            }
        });
        let _ = manager.register(Arc::clone(&line) as Arc<dyn Channel>);
        tokio::spawn(async move {
            if let Err(e) = line.run().await {
                error!("line channel: {e:#}");
            }
        });
        info!(account = %acct_for_log, "line channel started (webhook mode)");
    } // end for line_accounts
}

fn start_zalo_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    zalo_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::zalo::ZaloChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    use crate::channel::zalo::ZaloChannel;

    let Some(zalo_cfg) = &config.channel.channels.zalo else {
        return;
    };
    if !zalo_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy from config (Zalo is DM-only, no group policy needed).
    let dm_policy = zalo_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = zalo_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("zalo", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("zalo".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, access_token) pairs.
    let mut zalo_accounts: Vec<(String, String)> = Vec::new();

    // Legacy: single token at top level.
    if let Some(token) = zalo_cfg
        .access_token
        .as_ref()
        .and_then(|t| t.as_plain())
        .map(str::to_owned)
        .or_else(|| std::env::var("ZALO_ACCESS_TOKEN").ok())
    {
        zalo_accounts.push(("default".to_owned(), token));
    }

    // Multi-account: channels.zalo.accounts.<name>.accessToken
    if let Some(accts) = &zalo_cfg.accounts {
        for (name, acct) in accts {
            if let Some(t) = acct.get("accessToken").and_then(|v| v.as_str()) {
                if !zalo_accounts.iter().any(|(_, et)| et == t) {
                    zalo_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }
    }

    if zalo_accounts.is_empty() {
        warn!("ZALO access_token not set, zalo disabled");
        return;
    }

    for (acct_name, access_token) in zalo_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Per-user inbound queue for Zalo.
        type ZaloItem = (String, String, Vec<crate::agent::registry::ImageAttachment>);
        let zalo_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<ZaloItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender_id: String,
                  text: String,
                  images: Vec<crate::agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let queues = Arc::clone(&zalo_user_queues);
                tokio::spawn(async move {
                    // DM policy check (Zalo is DM-only).
                    {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&sender_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %sender_id, "zalo DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: sender_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: sender_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&sender_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&sender_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<ZaloItem>(32);
                            map.insert(sender_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_tx = tx.clone();
                            let w_uid = sender_id.clone();
                            tokio::spawn(async move {
                                while let Some((text, sender_id, images)) = urx.recv().await {
                                    let process_result = tokio::time::timeout(
                                    Duration::from_secs(600),
                                    async {
                                let handle = match w_reg.route("zalo") {
                                    Ok(h) => h,
                                    Err(e) => { error!("zalo route: {e:#}"); return; }
                                };
                                let dm_scope = default_dm_scope(&w_cfg);
                                let session_key = derive_session_key(&SessionKeyParams {
                                    agent_id: handle.id.clone(),
                                    kind: MessageKind::DirectMessage { account_id: None },
                                    channel: "zalo".to_string(),
                                    peer_id: sender_id.clone(),
                                    dm_scope,
                                });
                                let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                let msg = AgentMessage {
                                    session_key,
                                    text,
                                    channel: "zalo".to_string(),
                                    peer_id: sender_id.clone(),
                                    chat_id: String::new(),
                                    reply_tx,
                                    extra_tools: vec![],
                                    images,
                                    files: vec![],
                                };
                                if handle.tx.send(msg).await.is_err() {
                                    return;
                                }
                                let reply = tokio::select! {
                                    result = &mut reply_rx => result,
                                    _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                        send_processing(&w_tx, sender_id.clone(), false, &w_cfg).await;
                                        reply_rx.await
                                    }
                                };
                                if let Ok(r) = reply {
                                    let pending = r.pending_analysis;
                                    if !r.is_empty {
                                        let _ = w_tx
                                            .send(OutboundMessage {
                                                target_id: sender_id.clone(),
                                                is_group: false,
                                                text: r.text,
                                                reply_to: None,
                                                images: r.images,
                                                files: r.files,
                                                channel: None,                                            })
                                            .await;
                                    }
                                    if let Some(analysis) = pending {
                                        handle_pending_analysis(
                                            analysis, Arc::clone(&handle), &w_tx,
                                            sender_id, false, &w_cfg,
                                        ).await;
                                    }
                                }
                                    }
                                ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "zalo: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "zalo: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&sender_id).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let sender_id = sender_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("zalo") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: sender_id,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let sender_id = sender_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("zalo") {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: MessageKind::DirectMessage { account_id: None },
                                channel: "zalo".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "zalo".to_string(),
                                peer_id: sender_id.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: sender_id,
                                        is_group: false,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) = user_tx.try_send((text, sender_id.clone(), images)) {
                        warn!(user = %sender_id, error = %e, "zalo: user queue full, dropping message");
                    }
                });
            },
        );

        let zalo = Arc::new(ZaloChannel::with_api_base(
            access_token,
            zalo_cfg.api_base.clone(),
            on_message,
        ));
        let _ = zalo_slot.set(Arc::clone(&zalo));
        let zalo_send = Arc::clone(&zalo);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = zalo_send.send(msg).await {
                    error!("zalo send: {e:#}");
                }
            }
        });
        let _ = manager.register(Arc::clone(&zalo) as Arc<dyn Channel>);
        tokio::spawn(async move {
            if let Err(e) = zalo.run().await {
                error!("zalo channel: {e:#}");
            }
        });
        info!(account = %acct_for_log, "zalo channel started (webhook mode)");
    } // end for zalo_accounts
}

fn start_signal_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    use crate::channel::signal::SignalChannel;

    let Some(sig_cfg) = &config.channel.channels.signal else {
        return;
    };
    if !sig_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = sig_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = sig_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = sig_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = sig_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("signal", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("signal".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, phone) pairs.
    let mut sig_accounts: Vec<(String, String)> = Vec::new();

    // Legacy: single phone at top level.
    if let Some(p) = &sig_cfg.phone {
        sig_accounts.push(("default".to_owned(), p.clone()));
    }

    // Multi-account: channels.signal.accounts.<name>.phone
    if let Some(accts) = &sig_cfg.accounts {
        for (name, acct) in accts {
            if let Some(p) = acct.get("phone").and_then(|v| v.as_str()) {
                if !sig_accounts.iter().any(|(_, ep)| ep == p) {
                    sig_accounts.push((name.clone(), p.to_owned()));
                }
            }
        }
    }

    if sig_accounts.is_empty() {
        warn!("signal.phone not set, signal disabled");
        return;
    }
    let sig_cli_path = sig_cfg.cli_path.clone();

    for (acct_name, phone) in sig_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let sig_cli_path = sig_cli_path.clone();
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for Signal.
        type SigItem = (String, String, bool);
        let sig_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<SigItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(move |sender: String, text: String, is_group: bool| {
            let reg = Arc::clone(&reg);
            let cfg = Arc::clone(&cfg_arc);
            let tx = out_tx.clone();
            let enforcer = Arc::clone(&enforcer);
            let group_policy = Arc::clone(&gp);
            let group_allow = Arc::clone(&ga);
            let queues = Arc::clone(&sig_user_queues);
            tokio::spawn(async move {
                // Group policy check.
                if is_group {
                    match group_policy.as_ref() {
                        crate::config::schema::GroupPolicy::Disabled => {
                            debug!("signal group message rejected: groupPolicy=disabled");
                            return;
                        }
                        crate::config::schema::GroupPolicy::Allowlist => {
                            if !group_allow.iter().any(|g| *g == sender) {
                                debug!("signal group message rejected: not in groupAllowFrom");
                                return;
                            }
                        }
                        crate::config::schema::GroupPolicy::Open => {}
                    }
                }
                // DM policy check.
                if !is_group {
                    use crate::channel::PolicyResult;
                    match enforcer.check(&sender).await {
                        PolicyResult::Allow => {}
                        PolicyResult::Deny => {
                            debug!(peer_id = %sender, "signal DM rejected by policy");
                            return;
                        }
                        PolicyResult::SendPairingCode(code) => {
                            let _ = tx
                                .send(OutboundMessage {
                                    target_id: sender.clone(),
                                    is_group: false,
                                    text: crate::i18n::t_fmt(
                                        "pairing_required",
                                        crate::i18n::default_lang(),
                                        &[("code", &code)],
                                    ),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,

                    files: vec![],                                })
                                .await;
                            return;
                        }
                        PolicyResult::PairingQueueFull => {
                            let _ = tx
                                .send(OutboundMessage {
                                    target_id: sender.clone(),
                                    is_group: false,
                                    text: crate::i18n::t(
                                        "pairing_queue_full",
                                        crate::i18n::default_lang(),
                                    )
                                    .to_owned(),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,

                    files: vec![],                                })
                                .await;
                            return;
                        }
                    }
                }
                // Get or create a per-user queue.
                let user_tx = {
                    let mut map = queues.lock().await;
                    let needs_create = match map.get(&sender) {
                        Some(existing) if !existing.is_closed() => false,
                        Some(_) => {
                            map.remove(&sender);
                            true
                        }
                        None => true,
                    };
                    if needs_create {
                        let (utx, mut urx) = mpsc::channel::<SigItem>(32);
                        map.insert(sender.clone(), utx.clone());
                        let w_reg = Arc::clone(&reg);
                        let w_cfg = Arc::clone(&cfg);
                        let w_tx = tx.clone();
                        let w_uid = sender.clone();
                        tokio::spawn(async move {
                            while let Some((text, sender, is_group)) = urx.recv().await {
                                let process_result = tokio::time::timeout(
                                Duration::from_secs(600),
                                async {
                            let handle = match w_reg.route("signal") {
                                Ok(h) => h,
                                Err(e) => { error!("signal route: {e:#}"); return; }
                            };
                            let dm_scope = default_dm_scope(&w_cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: sender.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "signal".to_string(),
                                peer_id: sender.clone(),
                                dm_scope,
                            });
                            let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "signal".to_string(),
                                peer_id: sender.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images: vec![],
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            let reply = tokio::select! {
                                result = &mut reply_rx => result,
                                _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                    send_processing(&w_tx, sender.clone(), is_group, &w_cfg).await;
                                    reply_rx.await
                                }
                            };
                            if let Ok(r) = reply {
                                let pending = r.pending_analysis;
                                if !r.is_empty {
                                    let _ = w_tx
                                        .send(OutboundMessage {
                                            target_id: sender.clone(),
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,                                        })
                                        .await;
                                }
                                if let Some(analysis) = pending {
                                    handle_pending_analysis(
                                        analysis, Arc::clone(&handle), &w_tx,
                                        sender, is_group, &w_cfg,
                                    ).await;
                                }
                            }
                                }
                            ).await;
                                if process_result.is_err() {
                                    warn!(user = %w_uid, "signal: message processing timed out (600s), skipping to next");
                                }
                            }
                            debug!(user = %w_uid, "signal: per-user worker stopped");
                        });
                        utx
                    } else {
                        map.get(&sender).unwrap().clone()
                    }
                };
                // /btw bypass: spawn directly, skip the per-user queue
                if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                    let reg = Arc::clone(&reg);
                    let tx = tx.clone();
                    let cfg = Arc::clone(&cfg);
                    let question = text[5..].to_owned();
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        let handle = match reg.route("signal") {
                            Ok(h) => h,
                            Err(_) => return,
                        };
                        if let Some(reply_text) =
                            btw_direct_call(&question, &handle.live_status, &handle.providers, &cfg)
                                .await
                        {
                            let _ = tx
                                .send(OutboundMessage {
                                    target_id: sender,
                                    is_group: false,
                                    text: format!("[/btw] {}", reply_text),
                                    reply_to: None,
                                    images: vec![],
                                    channel: None,

                    files: vec![],                                })
                                .await;
                        }
                    });
                    return;
                }
                // Fast preparse bypass: local commands skip per-user queue
                if is_fast_preparse(&text) {
                    let reg = Arc::clone(&reg);
                    let tx = tx.clone();
                    let cfg = Arc::clone(&cfg);
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        let handle = match reg.route("signal") {
                            Ok(h) => h,
                            Err(_) => return,
                        };
                        let dm_scope = default_dm_scope(&cfg);
                        let session_key = derive_session_key(&SessionKeyParams {
                            agent_id: handle.id.clone(),
                            kind: if is_group {
                                MessageKind::GroupMessage {
                                    group_id: sender.clone(),
                                    thread_id: None,
                                }
                            } else {
                                MessageKind::DirectMessage { account_id: None }
                            },
                            channel: "signal".to_string(),
                            peer_id: sender.clone(),
                            dm_scope,
                        });
                        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                        let msg = AgentMessage {
                            session_key,
                            text,
                            channel: "signal".to_string(),
                            peer_id: sender.clone(),
                            chat_id: String::new(),
                            reply_tx,
                            extra_tools: vec![],
                            images: vec![],
                            files: vec![],
                        };
                        if handle.tx.send(msg).await.is_err() {
                            return;
                        }
                        if let Ok(r) = reply_rx.await {
                            if !r.is_empty {
                                let _ = tx.send(OutboundMessage {
                                    target_id: sender,
                                    is_group,
                                    text: r.text,
                                    reply_to: None,
                                    images: r.images,
                                    files: r.files,
                                    channel: None,
                                }).await;
                            }
                        }
                    });
                    return;
                }
                if let Err(e) = user_tx.try_send((text, sender.clone(), is_group)) {
                    warn!(user = %sender, error = %e, "signal: user queue full, dropping message");
                }
            });
        });

        // spawn() is async — drive it in a task.
        tokio::spawn(async move {
            match SignalChannel::spawn(phone, sig_cli_path, on_message).await {
                Ok(ch) => {
                    let ch = Arc::new(ch);
                    let ch_send = Arc::clone(&ch);
                    tokio::spawn(async move {
                        while let Some(msg) = out_rx.recv().await {
                            if let Err(e) = ch_send.send(msg).await {
                                error!("signal send: {e:#}");
                            }
                        }
                    });
                    info!(account = %acct_for_log, "signal channel started");
                    if let Err(e) = ch.run().await {
                        error!("signal channel: {e:#}");
                    }
                }
                Err(e) => warn!("signal-cli not available: {e:#}"),
            }
        });

        // Register a placeholder so ChannelManager knows signal is configured.
        // The real channel handle is inside the spawned task above.
        let _ = manager; // manager.register() can't be called here without the real Arc
    } // end for sig_accounts
}


// ---------------------------------------------------------------------------
// WeChat Personal (via ilink)
// ---------------------------------------------------------------------------

/// Per-user sequential message processor for WeChat.
/// Drains the user's inbound queue one message at a time, sends to agent,
/// waits for reply, then sends reply back via the outbound channel.
fn spawn_wechat_user_worker(
    user_id: String,
    mut rx: mpsc::Receiver<(
        String,
        Vec<crate::agent::registry::ImageAttachment>,
        Vec<crate::agent::registry::FileAttachment>,
    )>,
    reg: Arc<AgentRegistry>,
    cfg: RuntimeConfig,
    out_tx: mpsc::Sender<OutboundMessage>,
) {
    tokio::spawn(async move {
        debug!(user = %user_id, "wechat: per-user worker started");
        while let Some((text, images, file_attachments)) = rx.recv().await {
            debug!(user = %user_id, text_start = %text.chars().take(30).collect::<String>(), "wechat: worker processing");
            let process_result = tokio::time::timeout(Duration::from_secs(600), async {
                let handle = match reg.route_account("wechat", Some("default")).or_else(|_| reg.default_agent()) {
                    Ok(h) => h,
                    Err(e) => {
                        error!("wechat route error: {e:#}");
                        return;
                    }
                };
                let dm_scope = default_dm_scope(&cfg);
                let session_key = derive_session_key(&SessionKeyParams {
                    agent_id: handle.id.clone(),
                    kind: MessageKind::DirectMessage { account_id: None },
                    channel: "wechat".to_string(),
                    peer_id: user_id.clone(),
                    dm_scope,
                });
                let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                let msg = AgentMessage {
                    session_key,
                    text,
                    channel: "wechat".to_string(),
                    peer_id: user_id.clone(),
                    chat_id: String::new(),
                    reply_tx,
                    extra_tools: vec![],
                    images,
                    files: file_attachments,
                };
                if handle.tx.send(msg).await.is_err() {
                    return;
                }
                let reply = tokio::select! {
                    result = &mut reply_rx => result,
                    _ = tokio::time::sleep(processing_timeout(&cfg)) => {
                        send_processing(&out_tx, user_id.clone(), false, &cfg).await;
                        reply_rx.await
                    }
                };
                match reply {
                    Ok(r) => {
                        info!(
                            user = %user_id,
                            text_len = r.text.len(),
                            images = r.images.len(),
                            "wechat: got agent reply"
                        );
                        let pending = r.pending_analysis;
                        if !r.text.is_empty() || !r.images.is_empty() || !r.files.is_empty() {
                            if let Err(e) = out_tx
                                .send(OutboundMessage {
                                    target_id: user_id.clone(),
                                    is_group: false,
                                    text: r.text,
                                    reply_to: None,
                                    images: r.images,
                                    files: r.files,
                                    channel: None,                                })
                                .await
                            {
                                error!("wechat: failed to queue reply: {e:#}");
                            }
                        }
                        if let Some(analysis) = pending {
                            handle_pending_analysis(
                                analysis,
                                Arc::clone(&handle),
                                &out_tx,
                                user_id.clone(),
                                false,
                                &cfg,
                            )
                            .await;
                        }
                    }
                    Err(_) => {
                        warn!(user = %user_id, "wechat: agent dropped reply channel");
                    }
                }
            })
            .await;
            if process_result.is_err() {
                warn!(user = %user_id, "wechat: message processing timed out (600s), skipping to next");
            }
        }
        debug!(user = %user_id, "wechat: per-user worker stopped");
    });
}

fn start_wechat_personal_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    // Check if wechat channel is enabled in config
    let enabled = config
        .channel
        .channels
        .wechat
        .as_ref()
        .map(|c| c.base.enabled.unwrap_or(true))
        .unwrap_or(false);

    // Also check for saved token even without explicit config
    let token_data = crate::channel::auth::load_token("wechat");
    let bot_token = if enabled {
        // Try config first, then saved token
        config
            .channel
            .channels
            .wechat
            .as_ref()
            .and_then(|c| c.bot_token.as_ref())
            .and_then(|t| t.as_plain().map(str::to_owned))
            .or_else(|| {
                token_data
                    .as_ref()
                    .and_then(|d| d.get("bot_token"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
    } else if token_data.is_some() {
        // No config but has saved token — auto-enable
        token_data
            .as_ref()
            .and_then(|d| d.get("bot_token"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    } else {
        None
    };

    // Collect (account_name, token) pairs.
    let mut wc_accounts: Vec<(String, String)> = Vec::new();

    if let Some(token) = bot_token {
        wc_accounts.push(("default".to_owned(), token));
    }

    // Multi-account: channels.wechat.accounts.<name>.botToken
    if let Some(accts) = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.accounts.as_ref())
    {
        for (name, acct) in accts {
            if let Some(t) = acct.get("botToken").and_then(|v| v.as_str()) {
                if !wc_accounts.iter().any(|(_, et)| et == t) {
                    wc_accounts.push((name.clone(), t.to_owned()));
                }
            }
        }
    }

    if wc_accounts.is_empty() {
        return;
    }

    // Load dmPolicy from config (WeChat is DM-only).
    let dm_policy = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.base.dm_policy.clone())
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.base.allow_from.clone())
        .unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("wechat", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("wechat".to_owned(), Arc::clone(&enforcer));
    }

    let wechat_base_url = config
        .channel
        .channels
        .wechat
        .as_ref()
        .and_then(|c| c.base_url.as_deref())
        .map(str::to_owned);

    for (_acct_name, token) in wc_accounts {
        let enforcer = Arc::clone(&enforcer);
        let wechat_base_url = wechat_base_url.clone();
        let reg = Arc::clone(&registry);
        let cfg = config.clone();

        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Per-user inbound queue: serializes messages so each user's messages
        // are processed one at a time, preventing reply channel drops when
        // multiple files/messages arrive in quick succession.
        type InboundItem = (
            String,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
        );
        let user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<InboundItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |from_user: String,
                  text: String,
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  file_attachments: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let tx = out_tx.clone();
                let cfg = cfg.clone();
                let queues = Arc::clone(&user_queues);
                let enforcer = Arc::clone(&enforcer);
                tokio::spawn(async move {
                    // DM policy check (WeChat is DM-only).
                    {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&from_user).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %from_user, "wechat DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: from_user.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: from_user.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        if let Some(existing) = map.get(&from_user) {
                            if !existing.is_closed() {
                                existing.clone()
                            } else {
                                // Channel closed, create new one.
                                map.remove(&from_user);
                                let (utx, urx) = mpsc::channel::<InboundItem>(32);
                                map.insert(from_user.clone(), utx.clone());
                                // Spawn per-user sequential processor.
                                spawn_wechat_user_worker(
                                    from_user.clone(),
                                    urx,
                                    Arc::clone(&reg),
                                    cfg.clone(),
                                    tx.clone(),
                                );
                                utx
                            }
                        } else {
                            let (utx, urx) = mpsc::channel::<InboundItem>(32);
                            map.insert(from_user.clone(), utx.clone());
                            spawn_wechat_user_worker(
                                from_user.clone(),
                                urx,
                                Arc::clone(&reg),
                                cfg.clone(),
                                tx.clone(),
                            );
                            utx
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let question = text[5..].to_owned();
                        let from_user = from_user.clone();
                        tokio::spawn(async move {
                            let handle = match reg.get("main").or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: from_user,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let from_user = from_user.clone();
                        tokio::spawn(async move {
                            let handle = match reg.get("main").or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: MessageKind::DirectMessage { account_id: None },
                                channel: "wechat".to_string(),
                                peer_id: from_user.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "wechat".to_string(),
                                peer_id: from_user.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: file_attachments,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: from_user,
                                        is_group: false,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    // Enqueue — never blocks the poll loop.
                    if let Err(e) = user_tx.try_send((text, images, file_attachments)) {
                        warn!(user = %from_user, error = %e, "wechat: user queue full, dropping message");
                    }
                });
            },
        );

        let wc = Arc::new({
            let ch = crate::channel::wechat::WeChatPersonalChannel::new(token, on_message);
            if let Some(url) = wechat_base_url {
                ch.with_base_url(url)
            } else {
                ch
            }
        });
        let _ = manager.register(Arc::clone(&wc) as Arc<dyn crate::channel::Channel>);
        let wc_send = Arc::clone(&wc);

        tokio::spawn(async move {
            debug!("wechat: outbound sender task started");
            while let Some(msg) = out_rx.recv().await {
                debug!(target = %msg.target_id, text_len = msg.text.len(), "wechat: sending reply");
                if let Err(e) = wc_send.send(msg).await {
                    error!("wechat send error: {e:#}");
                } else {
                    debug!("wechat: reply sent successfully");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = wc.run().await {
                error!("wechat channel error: {e:#}");
            }
        });

        info!("wechat personal channel started");
    } // end for wc_accounts
}

// ---------------------------------------------------------------------------
// Feishu (飞书)
// ---------------------------------------------------------------------------

fn start_feishu_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    feishu_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::feishu::FeishuChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    let fs_cfg = config.channel.channels.feishu.as_ref();
    if let Some(cfg) = fs_cfg {
        if !cfg.base.enabled.unwrap_or(true) {
            return;
        }
    }

    // Collect (account_name, app_id, app_secret, brand) tuples.
    let mut fs_accounts: Vec<(String, String, String, String)> = Vec::new();

    // Legacy: single appId/appSecret at top level.
    if let Some(cfg) = fs_cfg {
        let id = cfg
            .app_id
            .as_deref()
            .filter(|s| !s.starts_with("YOUR_"))
            .map(str::to_owned);
        let secret = cfg
            .app_secret
            .as_ref()
            .and_then(|s| s.as_plain())
            .filter(|s| !s.starts_with("YOUR_"))
            .map(str::to_owned);
        let brand = cfg.brand.as_deref().unwrap_or("feishu").to_owned();
        if let (Some(id), Some(secret)) = (id, secret) {
            fs_accounts.push(("default".to_owned(), id, secret, brand));
        }
    }

    // Saved auth token from onboard flow (fallback for legacy single-account).
    if fs_accounts.is_empty() {
        if let Some(saved) = crate::channel::auth::load_token("feishu") {
            let id = saved["app_id"].as_str().unwrap_or("").to_owned();
            let secret = saved["app_secret"].as_str().unwrap_or("").to_owned();
            let brand = saved["brand"].as_str().unwrap_or("feishu").to_owned();
            if !id.is_empty() && !secret.is_empty() {
                fs_accounts.push(("default".to_owned(), id, secret, brand));
            }
        }
    }

    // Multi-account: channels.feishu.accounts.<name>.{appId, appSecret, brand?}
    if let Some(accts) = fs_cfg.and_then(|c| c.accounts.as_ref()) {
        for (name, acct) in accts {
            let id = acct.get("appId").and_then(|v| v.as_str()).unwrap_or("");
            let secret = acct.get("appSecret").and_then(|v| v.as_str()).unwrap_or("");
            if !id.is_empty() && !secret.is_empty() {
                // Avoid duplicate if top-level credentials == this account's.
                if !fs_accounts.iter().any(|(_, eid, _, _)| eid == id) {
                    let brand = acct
                        .get("brand")
                        .and_then(|v| v.as_str())
                        .unwrap_or("feishu")
                        .to_owned();
                    fs_accounts.push((name.clone(), id.to_owned(), secret.to_owned(), brand));
                }
            }
        }
    }

    if fs_accounts.is_empty() {
        // No config section and no saved token — silently skip.
        if fs_cfg.is_some() {
            warn!("feishu credentials not set, channel disabled");
        }
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = fs_cfg
        .and_then(|c| c.base.dm_policy.clone())
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = fs_cfg
        .and_then(|c| c.base.group_policy.clone())
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = fs_cfg
        .and_then(|c| c.base.group_allow_from.clone())
        .unwrap_or_default();
    let allow_from: Vec<String> = fs_cfg
        .and_then(|c| c.base.allow_from.clone())
        .unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("feishu", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("feishu".to_owned(), Arc::clone(&enforcer));
    }

    let feishu_api_base = fs_cfg.and_then(|c| c.api_base.clone());
    let feishu_ws_url = fs_cfg.and_then(|c| c.ws_url.clone());
    let max_file_size = config
        .ext
        .tools
        .as_ref()
        .and_then(|t| t.upload.as_ref())
        .and_then(|u| u.max_file_size)
        .unwrap_or(128_000_000);

    for (acct_name, app_id, app_secret, brand) in fs_accounts {
        let reg = Arc::clone(&registry);
        let cfg = config.clone();
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Register channel sender for notification routing (ACP tools like OpenCode, ClaudeCode)
        {
            let mut senders = _channel_senders.write().unwrap();
            // Register both "feishu" (for legacy/simple routing) and "feishu/{account}" (for multi-account)
            senders.insert("feishu".to_string(), out_tx.clone());
            senders.insert(format!("feishu/{}", acct_name), out_tx.clone());
        }

        // Find binding for this account to determine which agent handles it.
        let bound_agent = config
            .agents
            .bindings
            .iter()
            .find(|b| {
                b.match_.channel.as_deref() == Some("feishu")
                    && b.match_.account_id.as_deref() == Some(&acct_name)
            })
            .map(|b| b.agent_id.clone());
        let bound = bound_agent.clone();
        let _acct_for_route = acct_name.clone();

        // Per-user inbound queue for Feishu.
        type FsItem = (
            String,
            String,
            String,
            bool,
            Option<String>,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
        );
        let fs_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<FsItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender_id: String,
                  text: String,
                  chat_id: String,
                  is_group: bool,
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  file_attachments: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = cfg.clone();
                let tx = out_tx.clone();
                let bound = bound.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&fs_user_queues);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("feishu group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == chat_id) {
                                    debug!("feishu group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&sender_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %sender_id, "feishu DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: chat_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: chat_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&sender_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&sender_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<FsItem>(32);
                            map.insert(sender_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = cfg.clone();
                            let w_tx = tx.clone();
                            let w_uid = sender_id.clone();
                            tokio::spawn(async move {
                                while let Some((
                                    text,
                                    sender_id,
                                    chat_id,
                                    is_group,
                                    bound,
                                    images,
                                    file_attachments,
                                )) = urx.recv().await
                                {
                                    let process_result = tokio::time::timeout(
                                        Duration::from_secs(172800), // 48 hours, matching OpenClaw default
                                        async {
                                    let handle = if let Some(ref agent_id) = bound {
                                        match w_reg.get(agent_id) {
                                            Ok(h) => h,
                                            Err(_) => match w_reg.route_account("feishu", None) {
                                                Ok(h) => h,
                                                Err(e) => { error!("feishu route error: {e:#}"); return; }
                                            },
                                        }
                                    } else {
                                        match w_reg.route_account("feishu", None) {
                                            Ok(h) => h,
                                            Err(e) => { error!("feishu route error: {e:#}"); return; }
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: if is_group {
                                            MessageKind::GroupMessage {
                                                group_id: chat_id.clone(),
                                                thread_id: None,
                                            }
                                        } else {
                                            MessageKind::DirectMessage { account_id: None }
                                        },
                                        channel: "feishu".to_string(),
                                        peer_id: sender_id.clone(),
                                        dm_scope,
                                    });
                                    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                    let msg = AgentMessage {
                                        session_key,
                                        text,
                                        channel: "feishu".to_string(),
                                        peer_id: sender_id.clone(),
                                        chat_id: String::new(),
                                        reply_tx,
                                        extra_tools: vec![],
                                        images,
                                        files: file_attachments,
                                    };
                                    if handle.tx.send(msg).await.is_err() {
                                        return;
                                    }
                                    let fs_target = if is_group { chat_id.clone() } else { chat_id.clone() };
                                    let reply = tokio::select! {
                                        result = &mut reply_rx => result,
                                        _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                            send_processing(&w_tx, fs_target.clone(), is_group, &w_cfg).await;
                                            reply_rx.await
                                        }
                                    };
                                    match reply {
                                        Ok(r) => {
                                            let pending = r.pending_analysis;
                                            if !r.text.is_empty() || !r.images.is_empty() || !r.files.is_empty() {
                                                let _ = w_tx
                                                    .send(OutboundMessage {
                                                        target_id: fs_target.clone(),
                                                        is_group,
                                                        text: r.text,
                                                        reply_to: None,
                                                        images: r.images,
                                                        files: r.files,
                                                        channel: None,                                                    })
                                                    .await;
                                            }
                                            if let Some(analysis) = pending {
                                                handle_pending_analysis(
                                                    analysis, Arc::clone(&handle), &w_tx,
                                                    fs_target, is_group, &w_cfg,
                                                ).await;
                                            }
                                        }
                                        _ => {}
                                    }
                                        }
                                    ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "feishu: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "feishu: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&sender_id).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let question = text[5..].to_owned();
                        let chat_id = chat_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route_account("feishu", None) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: chat_id,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let sender_id = sender_id.clone();
                        let chat_id = chat_id.clone();
                        let bound = bound.clone();
                        tokio::spawn(async move {
                            let handle = if let Some(ref agent_id) = bound {
                                match reg.get(agent_id) {
                                    Ok(h) => h,
                                    Err(_) => match reg.route_account("feishu", None) {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    },
                                }
                            } else {
                                match reg.route_account("feishu", None) {
                                    Ok(h) => h,
                                    Err(_) => return,
                                }
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: chat_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "feishu".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "feishu".to_string(),
                                peer_id: sender_id,
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: file_attachments,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: chat_id,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) = user_tx.try_send((
                        text,
                        sender_id.clone(),
                        chat_id,
                        is_group,
                        bound,
                        images,
                        file_attachments,
                    )) {
                        warn!(user = %sender_id, error = %e, "feishu: user queue full, dropping message");
                    }
                });
            },
        );

        let mut fs_channel =
            crate::channel::feishu::FeishuChannel::new(app_id, app_secret, vec![], on_message);
        fs_channel.brand = brand;
        fs_channel.api_base_override = feishu_api_base.clone();
        fs_channel.ws_url_override = feishu_ws_url.clone();
        fs_channel.max_file_size = max_file_size;
        let fs = Arc::new(fs_channel);

        // First account fills the webhook slot for backward compatibility.
        let _ = feishu_slot.set(Arc::clone(&fs));
        let _ = manager.register(Arc::clone(&fs) as Arc<dyn crate::channel::Channel>);

        let fs_send = Arc::clone(&fs);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = fs_send.send(msg).await {
                    error!("feishu send error: {e:#}");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = fs.run().await {
                error!("feishu channel error: {e:#}");
            }
        });

        info!(account = %acct_for_log, "feishu channel started");
    }
}

// ---------------------------------------------------------------------------
// DingTalk (钉钉)
// ---------------------------------------------------------------------------

fn start_dingtalk_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    let Some(dt_cfg) = &config.channel.channels.dingtalk else {
        return;
    };
    if !dt_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Collect (account_name, app_key, app_secret, robot_code) tuples.
    let mut dt_accounts: Vec<(String, String, String, String)> = Vec::new();

    // Legacy: single appKey/appSecret at top level.
    if let (Some(key), Some(secret)) = (
        dt_cfg
            .app_key
            .as_deref()
            .filter(|s| !s.starts_with("YOUR_")),
        dt_cfg
            .app_secret
            .as_ref()
            .and_then(|s| s.as_plain())
            .filter(|s| !s.starts_with("YOUR_")),
    ) {
        let robot = dt_cfg.robot_code.clone().unwrap_or_else(|| key.to_owned());
        dt_accounts.push((
            "default".to_owned(),
            key.to_owned(),
            secret.to_owned(),
            robot,
        ));
    }

    // Multi-account: channels.dingtalk.accounts.<name>.{appKey, appSecret,
    // robotCode?}
    if let Some(accts) = &dt_cfg.accounts {
        for (name, acct) in accts {
            let key = acct.get("appKey").and_then(|v| v.as_str()).unwrap_or("");
            let secret = acct.get("appSecret").and_then(|v| v.as_str()).unwrap_or("");
            if !key.is_empty() && !secret.is_empty() {
                if !dt_accounts.iter().any(|(_, ek, _, _)| ek == key) {
                    let robot = acct
                        .get("robotCode")
                        .and_then(|v| v.as_str())
                        .unwrap_or(key)
                        .to_owned();
                    dt_accounts.push((name.clone(), key.to_owned(), secret.to_owned(), robot));
                }
            }
        }
    }

    if dt_accounts.is_empty() {
        warn!("dingtalk appKey not set, channel disabled");
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = dt_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = dt_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = dt_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = dt_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("dingtalk", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("dingtalk".to_owned(), Arc::clone(&enforcer));
    }

    for (acct_name, app_key, app_secret, robot_code) in dt_accounts {
        let reg = Arc::clone(&registry);
        let cfg = config.clone();
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Find binding for this account to determine which agent handles it.
        let bound_agent = config
            .agents
            .bindings
            .iter()
            .find(|b| {
                b.match_.channel.as_deref() == Some("dingtalk")
                    && b.match_.account_id.as_deref() == Some(&acct_name)
            })
            .map(|b| b.agent_id.clone());
        let bound = bound_agent.clone();

        // Per-user inbound queue for DingTalk.
        type DtItem = (
            String,
            String,
            String,
            bool,
            Option<String>,
            Vec<crate::agent::registry::ImageAttachment>,
        );
        let dt_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<DtItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender_id: String,
                  text: String,
                  conversation_id: String,
                  is_group: bool,
                  images: Vec<crate::agent::registry::ImageAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = cfg.clone();
                let tx = out_tx.clone();
                let bound = bound.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&dt_user_queues);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("dingtalk group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == conversation_id) {
                                    debug!(
                                        "dingtalk group message rejected: not in groupAllowFrom"
                                    );
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&sender_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %sender_id, "dingtalk DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: sender_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: sender_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&sender_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&sender_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<DtItem>(32);
                            map.insert(sender_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = cfg.clone();
                            let w_tx = tx.clone();
                            let w_uid = sender_id.clone();
                            tokio::spawn(async move {
                                while let Some((
                                    text,
                                    sender_id,
                                    conversation_id,
                                    is_group,
                                    bound,
                                    images,
                                )) = urx.recv().await
                                {
                                    let process_result = tokio::time::timeout(
                                        Duration::from_secs(172800), // 48 hours, matching OpenClaw default
                                        async {
                                    let handle = if let Some(ref agent_id) = bound {
                                        match w_reg.get(agent_id) {
                                            Ok(h) => h,
                                            Err(_) => match w_reg.route_account("dingtalk", None) {
                                                Ok(h) => h,
                                                Err(e) => { error!("dingtalk route error: {e:#}"); return; }
                                            },
                                        }
                                    } else {
                                        match w_reg.route_account("dingtalk", None) {
                                            Ok(h) => h,
                                            Err(e) => { error!("dingtalk route error: {e:#}"); return; }
                                        }
                                    };
                                    let dm_scope = default_dm_scope(&w_cfg);
                                    let session_key = derive_session_key(&SessionKeyParams {
                                        agent_id: handle.id.clone(),
                                        kind: if is_group {
                                            MessageKind::GroupMessage {
                                                group_id: conversation_id.clone(),
                                                thread_id: None,
                                            }
                                        } else {
                                            MessageKind::DirectMessage { account_id: None }
                                        },
                                        channel: "dingtalk".to_string(),
                                        peer_id: sender_id.clone(),
                                        dm_scope,
                                    });
                                    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                    let msg = AgentMessage {
                                        session_key,
                                        text,
                                        channel: "dingtalk".to_string(),
                                        peer_id: sender_id.clone(),
                                        chat_id: String::new(),
                                        reply_tx,
                                        extra_tools: vec![],
                                        images,
                                        files: vec![],
                                    };
                                    if handle.tx.send(msg).await.is_err() {
                                        return;
                                    }
                                    let dt_target = if is_group { conversation_id.clone() } else { sender_id.clone() };
                                    let reply = tokio::select! {
                                        result = &mut reply_rx => result,
                                        _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                            send_processing(&w_tx, dt_target.clone(), is_group, &w_cfg).await;
                                            reply_rx.await
                                        }
                                    };
                                    match reply {
                                        Ok(r) => {
                                            let pending = r.pending_analysis;
                                            if !r.text.is_empty() || !r.images.is_empty() || !r.files.is_empty() {
                                                let _ = w_tx
                                                    .send(OutboundMessage {
                                                        target_id: dt_target.clone(),
                                                        is_group,
                                                        text: r.text,
                                                        reply_to: None,
                                                        images: r.images,
                                                        files: r.files,
                                                        channel: None,                                                    })
                                                    .await;
                                            }
                                            if let Some(analysis) = pending {
                                                handle_pending_analysis(
                                                    analysis, Arc::clone(&handle), &w_tx,
                                                    dt_target, is_group, &w_cfg,
                                                ).await;
                                            }
                                        }
                                        _ => {}
                                    }
                                        }
                                    ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "dingtalk: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "dingtalk: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&sender_id).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let question = text[5..].to_owned();
                        let sender_id = sender_id.clone();
                        let conversation_id = conversation_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route_account("dingtalk", None) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let target = if is_group { conversation_id } else { sender_id };
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: target,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let sender_id = sender_id.clone();
                        let conversation_id = conversation_id.clone();
                        let bound = bound.clone();
                        tokio::spawn(async move {
                            let handle = if let Some(ref agent_id) = bound {
                                match reg.get(agent_id) {
                                    Ok(h) => h,
                                    Err(_) => match reg.route_account("dingtalk", None) {
                                        Ok(h) => h,
                                        Err(_) => return,
                                    },
                                }
                            } else {
                                match reg.route_account("dingtalk", None) {
                                    Ok(h) => h,
                                    Err(_) => return,
                                }
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: conversation_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "dingtalk".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "dingtalk".to_string(),
                                peer_id: sender_id.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: vec![],
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let target = if is_group { conversation_id } else { sender_id };
                                    let _ = tx.send(OutboundMessage {
                                        target_id: target,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) = user_tx.try_send((
                        text,
                        sender_id.clone(),
                        conversation_id,
                        is_group,
                        bound,
                        images,
                    )) {
                        warn!(user = %sender_id, error = %e, "dingtalk: user queue full, dropping message");
                    }
                });
            },
        );

        let dt = Arc::new(crate::channel::dingtalk::DingTalkChannel::new(
            app_key,
            app_secret,
            robot_code,
            dt_cfg.api_base.clone(),
            dt_cfg.oapi_base.clone(),
            on_message,
        ));
        let _ = manager.register(Arc::clone(&dt) as Arc<dyn crate::channel::Channel>);
        let dt_send = Arc::clone(&dt);

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = dt_send.send(msg).await {
                    error!("dingtalk send error: {e:#}");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = dt.run().await {
                error!("dingtalk channel error: {e:#}");
            }
        });

        info!(account = %acct_for_log, "dingtalk channel started");
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

fn start_qq_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    let Some(qq_cfg) = &config.channel.channels.qq else {
        return;
    };
    if !qq_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = qq_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = qq_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> = qq_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = qq_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("qq", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("qq".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, app_id, app_secret) tuples.
    let mut qq_accounts: Vec<(String, String, String)> = Vec::new();

    // Legacy: single appId/appSecret at top level.
    if let (Some(id), Some(secret)) = (
        qq_cfg.app_id.as_deref().filter(|s| !s.is_empty()),
        qq_cfg
            .app_secret
            .as_ref()
            .and_then(|s| s.as_plain())
            .filter(|s| !s.is_empty()),
    ) {
        qq_accounts.push(("default".to_owned(), id.to_owned(), secret.to_owned()));
    }

    // Multi-account: channels.qq.accounts.<name>.{appId, appSecret}
    if let Some(accts) = &qq_cfg.accounts {
        for (name, acct) in accts {
            let id = acct.get("appId").and_then(|v| v.as_str()).unwrap_or("");
            let secret = acct.get("appSecret").and_then(|v| v.as_str()).unwrap_or("");
            if !id.is_empty() && !secret.is_empty() {
                if !qq_accounts.iter().any(|(_, eid, _)| eid == id) {
                    qq_accounts.push((name.clone(), id.to_owned(), secret.to_owned()));
                }
            }
        }
    }

    if qq_accounts.is_empty() {
        warn!("qq appId not set, channel disabled");
        return;
    }

    let sandbox = qq_cfg.sandbox.unwrap_or(false);
    let intents = qq_cfg.intents;
    let qq_api_base = qq_cfg.api_base.clone();
    let qq_token_url = qq_cfg.token_url.clone();

    for (acct_name, app_id, app_secret) in qq_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let qq_cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for QQ.
        type QqItem = (
            String,
            String,
            String,
            bool,
            String,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
        );
        let qq_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<QqItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender_id: String,
                  text: String,
                  target_id: String,
                  is_group: bool,
                  msg_id: String,
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  file_attachments: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let tx = out_tx.clone();
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                let queues = Arc::clone(&qq_user_queues);
                let qq_cfg = Arc::clone(&qq_cfg_arc);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("qq group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == target_id) {
                                    debug!("qq group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&sender_id).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %sender_id, "qq DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: target_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: target_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&sender_id) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&sender_id);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<QqItem>(32);
                            map.insert(sender_id.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_tx = tx.clone();
                            let w_uid = sender_id.clone();
                            let w_cfg = Arc::clone(&qq_cfg);
                            tokio::spawn(async move {
                                while let Some((
                                    text,
                                    sender_id,
                                    target_id,
                                    is_group,
                                    msg_id,
                                    images,
                                    file_attachments,
                                )) = urx.recv().await
                                {
                                    let process_result = tokio::time::timeout(
                                    Duration::from_secs(600),
                                    async {
                                let handle = match w_reg.route("qq").or_else(|_| w_reg.default_agent()) {
                                    Ok(h) => h,
                                    Err(e) => { error!("qq route error: {e:#}"); return; }
                                };
                                let session_key =
                                    format!("qq:{}:{}", if is_group { "group" } else { "dm" }, target_id);
                                let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                                let msg = crate::agent::AgentMessage {
                                    session_key,
                                    text,
                                    channel: "qq".to_owned(),
                                    peer_id: sender_id,
                                    chat_id: target_id.clone(),
                                    reply_tx,
                                    extra_tools: vec![],
                                    images,
                                    files: file_attachments,
                                };
                                if handle.tx.send(msg).await.is_err() {
                                    error!("qq: agent inbox closed");
                                    return;
                                }
                                let reply = tokio::select! {
                                    result = &mut reply_rx => result,
                                    _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                        send_processing(&w_tx, target_id.clone(), is_group, &w_cfg).await;
                                        reply_rx.await
                                    }
                                };
                                match reply {
                                    Ok(r) => {
                                        let pending = r.pending_analysis;
                                        let _ = w_tx
                                            .send(OutboundMessage {
                                                target_id: target_id.clone(),
                                                is_group,
                                                text: r.text,
                                                reply_to: Some(msg_id),
                                                images: r.images,
                                                files: r.files,
                                                channel: None,                                            })
                                            .await;
                                        if let Some(analysis) = pending {
                                            handle_pending_analysis(
                                                analysis, Arc::clone(&handle), &w_tx,
                                                target_id, is_group, &w_cfg,
                                            ).await;
                                        }
                                    }
                                    Err(_) => error!("qq: agent dropped reply"),
                                }
                                    }
                                ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "qq: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "qq: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&sender_id).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let qq_cfg = Arc::clone(&qq_cfg);
                        let question = text[5..].to_owned();
                        let target_id = target_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("qq").or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &qq_cfg,
                            )
                            .await
                            {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let qq_cfg = Arc::clone(&qq_cfg);
                        let sender_id = sender_id.clone();
                        let target_id = target_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("qq").or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&qq_cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: target_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "qq".to_string(),
                                peer_id: sender_id.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "qq".to_string(),
                                peer_id: sender_id,
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files: file_attachments,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) = user_tx.try_send((
                        text,
                        sender_id.clone(),
                        target_id,
                        is_group,
                        msg_id,
                        images,
                        file_attachments,
                    )) {
                        warn!(user = %sender_id, error = %e, "qq: user queue full, dropping message");
                    }
                });
            },
        );

        let qq = Arc::new(crate::channel::qq::QQBotChannel::new_with_overrides(
            app_id,
            app_secret,
            sandbox,
            intents,
            on_message,
            qq_api_base.clone(),
            qq_token_url.clone(),
        ));

        let _ = manager.register(Arc::clone(&qq) as Arc<dyn crate::channel::Channel>);
        let qq_send = Arc::clone(&qq);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = qq_send.send(msg).await {
                    error!("qq send error: {e:#}");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = qq.run().await {
                error!("qq channel error: {e:#}");
            }
        });

        info!(account = %acct_for_log, "qq bot channel started");
    } // end for qq_accounts
}

fn start_matrix_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    let Some(matrix_cfg) = &config.channel.channels.matrix else {
        return;
    };
    if !matrix_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Load dmPolicy and groupPolicy from config.
    let dm_policy = matrix_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let group_policy = matrix_cfg
        .base
        .group_policy
        .clone()
        .unwrap_or(crate::config::schema::GroupPolicy::Allowlist);
    let group_allow_from: Vec<String> =
        matrix_cfg.base.group_allow_from.clone().unwrap_or_default();
    let allow_from: Vec<String> = matrix_cfg.base.allow_from.clone().unwrap_or_default();

    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("matrix", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("matrix".to_owned(), Arc::clone(&enforcer));
    }

    // Collect (account_name, homeserver, access_token, user_id) tuples.
    let mut mx_accounts: Vec<(String, String, String, String)> = Vec::new();

    // Legacy: single credentials at top level.
    if let Some(token) = matrix_cfg
        .access_token
        .as_ref()
        .and_then(|s| s.resolve_early())
        .filter(|s| !s.is_empty())
    {
        let hs = matrix_cfg
            .homeserver
            .as_deref()
            .unwrap_or("https://matrix.org")
            .to_owned();
        let uid = matrix_cfg.user_id.as_deref().unwrap_or("").to_owned();
        mx_accounts.push(("default".to_owned(), hs, token, uid));
    }

    // Multi-account: channels.matrix.accounts.<name>.{homeserver?, accessToken,
    // userId?}
    if let Some(accts) = &matrix_cfg.accounts {
        for (name, acct) in accts {
            let token = acct
                .get("accessToken")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !token.is_empty() && !mx_accounts.iter().any(|(_, _, et, _)| et == token) {
                let hs = acct
                    .get("homeserver")
                    .and_then(|v| v.as_str())
                    .unwrap_or("https://matrix.org")
                    .to_owned();
                let uid = acct
                    .get("userId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                mx_accounts.push((name.clone(), hs, token.to_owned(), uid));
            }
        }
    }

    if mx_accounts.is_empty() {
        warn!("matrix accessToken not set, channel disabled");
        return;
    }

    for (acct_name, homeserver, access_token, user_id) in mx_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg = config.clone();
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        let gp = Arc::new(group_policy.clone());
        let ga = Arc::new(group_allow_from.clone());

        // Per-user inbound queue for Matrix.
        type MatrixItem = (
            String,
            String,
            String,
            bool,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
        );
        let matrix_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<MatrixItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let on_message = Arc::new(
            move |sender: String,
                  text: String,
                  room_id: String,
                  is_group: bool,
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  files: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let tx = out_tx.clone();
                let cfg = cfg.clone();
                let queues = Arc::clone(&matrix_user_queues);
                let enforcer = Arc::clone(&enforcer);
                let group_policy = Arc::clone(&gp);
                let group_allow = Arc::clone(&ga);
                tokio::spawn(async move {
                    // Group policy check.
                    if is_group {
                        match group_policy.as_ref() {
                            crate::config::schema::GroupPolicy::Disabled => {
                                debug!("matrix group message rejected: groupPolicy=disabled");
                                return;
                            }
                            crate::config::schema::GroupPolicy::Allowlist => {
                                if !group_allow.iter().any(|g| *g == room_id) {
                                    debug!("matrix group message rejected: not in groupAllowFrom");
                                    return;
                                }
                            }
                            crate::config::schema::GroupPolicy::Open => {}
                        }
                    }
                    // DM policy check.
                    if !is_group {
                        use crate::channel::PolicyResult;
                        match enforcer.check(&sender).await {
                            PolicyResult::Allow => {}
                            PolicyResult::Deny => {
                                debug!(peer_id = %sender, "matrix DM rejected by policy");
                                return;
                            }
                            PolicyResult::SendPairingCode(code) => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: room_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t_fmt(
                                            "pairing_required",
                                            crate::i18n::default_lang(),
                                            &[("code", &code)],
                                        ),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            PolicyResult::PairingQueueFull => {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: room_id.clone(),
                                        is_group: false,
                                        text: crate::i18n::t(
                                            "pairing_queue_full",
                                            crate::i18n::default_lang(),
                                        )
                                        .to_owned(),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&sender) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&sender);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<MatrixItem>(32);
                            map.insert(sender.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = cfg.clone();
                            let w_tx = tx.clone();
                            let w_uid = sender.clone();
                            tokio::spawn(async move {
                                while let Some((text, sender, room_id, is_group, images, files)) =
                                    urx.recv().await
                                {
                                    let process_result = tokio::time::timeout(
                                Duration::from_secs(600),
                                async {
                            let handle = match w_reg.route("matrix").or_else(|_| w_reg.default_agent()) {
                                Ok(h) => h,
                                Err(e) => { error!("matrix route error: {e:#}"); return; }
                            };
                            let dm_scope = default_dm_scope(&w_cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: room_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "matrix".to_string(),
                                peer_id: sender.clone(),
                                dm_scope,
                            });
                            let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                            let msg = crate::agent::AgentMessage {
                                session_key,
                                text,
                                channel: "matrix".to_owned(),
                                peer_id: sender.clone(),
                                chat_id: room_id.clone(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            let reply = tokio::select! {
                                result = &mut reply_rx => result,
                                _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                    send_processing(&w_tx, room_id.clone(), is_group, &w_cfg).await;
                                    reply_rx.await
                                }
                            };
                            match reply {
                                Ok(r) => {
                                    let pending = r.pending_analysis;
                                    if !r.is_empty {
                                        let _ = w_tx.send(OutboundMessage {
                                            target_id: room_id.clone(),
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,                                        }).await;
                                    }
                                    if let Some(analysis) = pending {
                                        handle_pending_analysis(
                                            analysis, Arc::clone(&handle), &w_tx,
                                            room_id, is_group, &w_cfg,
                                        ).await;
                                    }
                                }
                                _ => {}
                            }
                                }
                            ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "matrix: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "matrix: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&sender).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let question = text[5..].to_owned();
                        let room_id = room_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("matrix").or_else(|_| reg.default_agent())
                            {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: room_id,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = cfg.clone();
                        let sender = sender.clone();
                        let room_id = room_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("matrix").or_else(|_| reg.default_agent())
                            {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: room_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "matrix".to_string(),
                                peer_id: sender.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "matrix".to_string(),
                                peer_id: sender,
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let _ = tx.send(OutboundMessage {
                                        target_id: room_id,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) =
                        user_tx.try_send((text, sender.clone(), room_id, is_group, images, files))
                    {
                        warn!(user = %sender, error = %e, "matrix: user queue full, dropping message");
                    }
                });
            },
        );

        let matrix = Arc::new({
            let ch = crate::channel::matrix::MatrixChannel::new(
                homeserver,
                access_token,
                user_id,
                on_message,
            );
            #[cfg(feature = "channel-matrix")]
            {
                if let Some(did) = matrix_cfg.device_id.as_deref() {
                    ch = ch.with_device_id(did);
                }
                if let Some(rk) = matrix_cfg
                    .recovery_key
                    .as_ref()
                    .and_then(|s| s.resolve_early())
                {
                    ch = ch.with_recovery_key(rk);
                }
            }
            ch
        });

        let _ = manager.register(Arc::clone(&matrix) as Arc<dyn crate::channel::Channel>);
        let matrix_send = Arc::clone(&matrix);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = matrix_send.send(msg).await {
                    error!("matrix send error: {e:#}");
                }
            }
        });

        tokio::spawn(async move {
            if let Err(e) = matrix.run().await {
                error!("matrix channel error: {e:#}");
            }
        });

        info!(account = %acct_for_log, "matrix channel started");
    } // end for mx_accounts
}

fn start_wecom_if_configured(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    wecom_slot: Arc<tokio::sync::OnceCell<Arc<crate::channel::wecom::WeComChannel>>>,
    dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    redb_store: Arc<crate::store::redb_store::RedbStore>,
    _channel_senders: Arc<
        std::sync::RwLock<std::collections::HashMap<String, mpsc::Sender<OutboundMessage>>>,
    >,
) {
    use crate::channel::wecom::WeComChannel;

    let Some(wc_cfg) = &config.channel.channels.wecom else {
        return;
    };
    if !wc_cfg.base.enabled.unwrap_or(true) {
        return;
    }

    // Collect (account_name, bot_id, secret, ws_url) tuples.
    let mut wc_accounts: Vec<(String, String, String, Option<String>)> = Vec::new();

    // Legacy: single bot_id/secret at top level.
    if let (Some(bot_id), Some(secret)) = (
        wc_cfg.bot_id.as_deref().filter(|s| !s.is_empty()),
        wc_cfg
            .secret
            .as_ref()
            .and_then(|s| s.resolve_early())
            .filter(|s| !s.is_empty()),
    ) {
        wc_accounts.push((
            "default".to_owned(),
            bot_id.to_owned(),
            secret,
            wc_cfg.ws_url.clone(),
        ));
    }

    // Multi-account: channels.wecom.accounts.<name>.{botId, secret, wsUrl?}
    if let Some(accts) = &wc_cfg.accounts {
        for (name, acct) in accts {
            let bid = acct.get("botId").and_then(|v| v.as_str()).unwrap_or("");
            let sec = acct.get("secret").and_then(|v| v.as_str()).unwrap_or("");
            if !bid.is_empty() && !sec.is_empty() {
                if !wc_accounts.iter().any(|(_, eid, _, _)| eid == bid) {
                    let ws = acct
                        .get("wsUrl")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                        .or_else(|| wc_cfg.ws_url.clone());
                    wc_accounts.push((name.clone(), bid.to_owned(), sec.to_owned(), ws));
                }
            }
        }
    }

    if wc_accounts.is_empty() {
        warn!("wecom bot_id not set, channel disabled");
        return;
    }

    // DM policy enforcement for WeCom.
    let dm_policy = wc_cfg
        .base
        .dm_policy
        .clone()
        .unwrap_or(crate::config::schema::DmPolicy::Pairing);
    let allow_from: Vec<String> = wc_cfg.base.allow_from.clone().unwrap_or_default();
    let enforcer = Arc::new(
        crate::channel::DmPolicyEnforcer::new(dm_policy, allow_from)
            .with_persistence("wecom", Arc::clone(&redb_store)),
    );
    if let Ok(mut enforcers) = dm_enforcers.write() {
        enforcers.insert("wecom".to_owned(), Arc::clone(&enforcer));
    }

    for (acct_name, bot_id, secret, ws_url) in wc_accounts {
        let acct_for_log = acct_name.clone();
        let enforcer = Arc::clone(&enforcer);
        let reg = Arc::clone(&registry);
        let cfg_arc = Arc::new(config.clone());
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

        // Per-user inbound queue for WeCom.
        type WcItem = (
            String,
            String,
            String,
            bool,
            Vec<crate::agent::registry::ImageAttachment>,
            Vec<crate::agent::registry::FileAttachment>,
        );
        let wc_user_queues: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<WcItem>>>,
        > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let wc_enforcer = Arc::clone(&enforcer);
        let on_message = Arc::new(
            move |from: String,
                  text: String,
                  chat_id: String,
                  is_group: bool,
                  images: Vec<crate::agent::registry::ImageAttachment>,
                  files: Vec<crate::agent::registry::FileAttachment>| {
                let reg = Arc::clone(&reg);
                let cfg = Arc::clone(&cfg_arc);
                let tx = out_tx.clone();
                let queues = Arc::clone(&wc_user_queues);
                let enforcer = Arc::clone(&wc_enforcer);
                tokio::spawn(async move {
                    // DM policy check (pairing).
                    if !is_group {
                        match enforcer.check(&from).await {
                            crate::channel::PolicyResult::Allow => {}
                            crate::channel::PolicyResult::SendPairingCode(code) => {
                                let lang = cfg
                                    .raw
                                    .gateway
                                    .as_ref()
                                    .and_then(|g| g.language.as_deref())
                                    .map(crate::i18n::resolve_lang)
                                    .unwrap_or("en");
                                let msg = crate::i18n::t_fmt(
                                    "pairing_required",
                                    lang,
                                    &[("code", &code)],
                                );
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: chat_id.clone(),
                                        is_group: false,
                                        text: msg,
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                                return;
                            }
                            crate::channel::PolicyResult::Deny
                            | crate::channel::PolicyResult::PairingQueueFull => {
                                debug!(from = %from, "wecom: DM blocked by policy");
                                return;
                            }
                        }
                    }
                    // Get or create a per-user queue.
                    let user_tx = {
                        let mut map = queues.lock().await;
                        let needs_create = match map.get(&from) {
                            Some(existing) if !existing.is_closed() => false,
                            Some(_) => {
                                map.remove(&from);
                                true
                            }
                            None => true,
                        };
                        if needs_create {
                            let (utx, mut urx) = mpsc::channel::<WcItem>(32);
                            map.insert(from.clone(), utx.clone());
                            let w_reg = Arc::clone(&reg);
                            let w_cfg = Arc::clone(&cfg);
                            let w_tx = tx.clone();
                            let w_uid = from.clone();
                            tokio::spawn(async move {
                                while let Some((text, from, chat_id, is_group, images, files)) =
                                    urx.recv().await
                                {
                                    let process_result = tokio::time::timeout(
                                Duration::from_secs(600),
                                async {
                            let handle = match w_reg.route("wecom").or_else(|_| w_reg.default_agent()) {
                                Ok(h) => h,
                                Err(e) => { error!("wecom route: {e:#}"); return; }
                            };
                            let dm_scope = default_dm_scope(&w_cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: chat_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "wecom".to_string(),
                                peer_id: from.clone(),
                                dm_scope,
                            });
                            let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
                            let msg = crate::agent::AgentMessage {
                                session_key,
                                text,
                                channel: "wecom".to_owned(),
                                peer_id: from.clone(),
                                chat_id: chat_id.clone(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            let target = if is_group { chat_id } else { from };
                            let reply = tokio::select! {
                                result = &mut reply_rx => result,
                                _ = tokio::time::sleep(processing_timeout(&w_cfg)) => {
                                    send_processing(&w_tx, target.clone(), is_group, &w_cfg).await;
                                    reply_rx.await
                                }
                            };
                            if let Ok(r) = reply {
                                let pending = r.pending_analysis;
                                if !r.is_empty {
                                    let _ = w_tx
                                        .send(OutboundMessage {
                                            target_id: target.clone(),
                                            is_group,
                                            text: r.text,
                                            reply_to: None,
                                            images: r.images,
                                            files: r.files,
                                            channel: None,                                        })
                                        .await;
                                }
                                if let Some(analysis) = pending {
                                    handle_pending_analysis(
                                        analysis, Arc::clone(&handle), &w_tx,
                                        target, is_group, &w_cfg,
                                    ).await;
                                }
                            }
                                }
                            ).await;
                                    if process_result.is_err() {
                                        warn!(user = %w_uid, "wecom: message processing timed out (600s), skipping to next");
                                    }
                                }
                                debug!(user = %w_uid, "wecom: per-user worker stopped");
                            });
                            utx
                        } else {
                            map.get(&from).unwrap().clone()
                        }
                    };
                    // /btw bypass: spawn directly, skip the per-user queue
                    if text.starts_with("/btw ") || text.starts_with("/BTW ") {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let question = text[5..].to_owned();
                        let from = from.clone();
                        let chat_id = chat_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("wecom").or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            if let Some(reply_text) = btw_direct_call(
                                &question,
                                &handle.live_status,
                                &handle.providers,
                                &cfg,
                            )
                            .await
                            {
                                let target = if is_group { chat_id } else { from };
                                let _ = tx
                                    .send(OutboundMessage {
                                        target_id: target,
                                        is_group: false,
                                        text: format!("[/btw] {}", reply_text),
                                        reply_to: None,
                                        images: vec![],
                                        channel: None,

                    files: vec![],                                    })
                                    .await;
                            }
                        });
                        return;
                    }
                    // Fast preparse bypass: local commands skip per-user queue
                    if is_fast_preparse(&text) {
                        let reg = Arc::clone(&reg);
                        let tx = tx.clone();
                        let cfg = Arc::clone(&cfg);
                        let from = from.clone();
                        let chat_id = chat_id.clone();
                        tokio::spawn(async move {
                            let handle = match reg.route("wecom").or_else(|_| reg.default_agent()) {
                                Ok(h) => h,
                                Err(_) => return,
                            };
                            let dm_scope = default_dm_scope(&cfg);
                            let session_key = derive_session_key(&SessionKeyParams {
                                agent_id: handle.id.clone(),
                                kind: if is_group {
                                    MessageKind::GroupMessage {
                                        group_id: chat_id.clone(),
                                        thread_id: None,
                                    }
                                } else {
                                    MessageKind::DirectMessage { account_id: None }
                                },
                                channel: "wecom".to_string(),
                                peer_id: from.clone(),
                                dm_scope,
                            });
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            let msg = AgentMessage {
                                session_key,
                                text,
                                channel: "wecom".to_string(),
                                peer_id: from.clone(),
                                chat_id: String::new(),
                                reply_tx,
                                extra_tools: vec![],
                                images,
                                files,
                            };
                            if handle.tx.send(msg).await.is_err() {
                                return;
                            }
                            if let Ok(r) = reply_rx.await {
                                if !r.is_empty {
                                    let target = if is_group { chat_id } else { from };
                                    let _ = tx.send(OutboundMessage {
                                        target_id: target,
                                        is_group,
                                        text: r.text,
                                        reply_to: None,
                                        images: r.images,
                                        files: r.files,
                                        channel: None,
                                    }).await;
                                }
                            }
                        });
                        return;
                    }
                    if let Err(e) =
                        user_tx.try_send((text, from.clone(), chat_id, is_group, images, files))
                    {
                        warn!(user = %from, error = %e, "wecom: user queue full, dropping message");
                    }
                });
            },
        );

        let wecom = Arc::new(WeComChannel::new(bot_id, secret, ws_url, on_message));

        let _ = manager.register(Arc::clone(&wecom) as Arc<dyn crate::channel::Channel>);
        let wecom_send = Arc::clone(&wecom);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = wecom_send.send(msg).await {
                    error!("wecom send: {e:#}");
                }
            }
        });

        // First account fills the webhook slot for backward compatibility.
        let _ = wecom_slot.set(Arc::clone(&wecom));

        tokio::spawn(async move {
            if let Err(e) = wecom.run().await {
                error!("wecom channel: {e:#}");
            }
        });

        info!(account = %acct_for_log, "wecom AI Bot WS channel started");
    } // end for wc_accounts
}

// ---------------------------------------------------------------------------
// Custom channels (webhook + websocket)
// ---------------------------------------------------------------------------

fn start_custom_channels(
    config: &RuntimeConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    custom_webhooks: Arc<
        std::sync::RwLock<
            std::collections::HashMap<String, Arc<crate::channel::custom::CustomWebhookChannel>>,
        >,
    >,
) {
    let custom_cfgs = match &config.channel.channels.custom {
        Some(cfgs) => cfgs,
        None => return,
    };

    for ch_cfg in custom_cfgs {
        if !ch_cfg.base.enabled.unwrap_or(true) {
            continue;
        }

        let ch_name = ch_cfg.name.clone();

        match ch_cfg.channel_type.as_str() {
            "webhook" => {
                start_custom_webhook(
                    config,
                    ch_cfg.clone(),
                    Arc::clone(&registry),
                    manager,
                    Arc::clone(&custom_webhooks),
                );
            }
            "websocket" => {
                start_custom_websocket(config, ch_cfg.clone(), Arc::clone(&registry), manager);
            }
            other => {
                warn!(
                    channel = %ch_name,
                    channel_type = %other,
                    "unknown custom channel type, skipping"
                );
            }
        }
    }
}

fn start_custom_webhook(
    config: &RuntimeConfig,
    ch_cfg: crate::config::schema::CustomChannelConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
    custom_webhooks: Arc<
        std::sync::RwLock<
            std::collections::HashMap<String, Arc<crate::channel::custom::CustomWebhookChannel>>,
        >,
    >,
) {
    use crate::channel::custom::CustomWebhookChannel;

    let ch_name = ch_cfg.name.clone();
    let reg = Arc::clone(&registry);
    let cfg_arc = Arc::new(config.clone());
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

    let ch_name_cb = ch_name.clone();
    let on_message = Arc::new(move |sender: String, text: String, is_group: bool| {
        let reg = Arc::clone(&reg);
        let cfg = Arc::clone(&cfg_arc);
        let tx = out_tx.clone();
        let ch_name = ch_name_cb.clone();
        tokio::spawn(async move {
            let handle = match reg.route(&ch_name) {
                Ok(h) => h,
                Err(e) => {
                    error!(channel = %ch_name, "route error: {e:#}");
                    return;
                }
            };
            let dm_scope = default_dm_scope(&cfg);
            let session_key = derive_session_key(&SessionKeyParams {
                agent_id: handle.id.clone(),
                kind: if is_group {
                    MessageKind::GroupMessage {
                        group_id: sender.clone(),
                        thread_id: None,
                    }
                } else {
                    MessageKind::DirectMessage { account_id: None }
                },
                channel: ch_name.clone(),
                peer_id: sender.clone(),
                dm_scope,
            });
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let msg = AgentMessage {
                session_key,
                text,
                channel: ch_name.clone(),
                peer_id: sender.clone(),
                chat_id: sender.clone(),
                reply_tx,
                extra_tools: vec![],
                images: vec![],
                files: vec![],
            };
            if handle.tx.send(msg).await.is_err() {
                return;
            }
            if let Ok(Ok(r)) = tokio::time::timeout(Duration::from_secs(120), reply_rx).await {
                let pending = r.pending_analysis;
                if !r.is_empty {
                    let _ = tx
                        .send(OutboundMessage {
                            target_id: sender.clone(),
                            is_group,
                            text: r.text,
                            reply_to: None,
                            images: r.images,
                            files: r.files,
                            channel: None,                        })
                        .await;
                }
                if let Some(analysis) = pending {
                    handle_pending_analysis(
                        analysis,
                        Arc::clone(&handle),
                        &tx,
                        sender,
                        is_group,
                        &cfg,
                    )
                    .await;
                }
            }
        });
    });

    let ch = Arc::new(CustomWebhookChannel::new(ch_cfg, on_message));

    // Register in the custom_webhooks map for /hooks/{name} dispatch.
    if let Ok(mut map) = custom_webhooks.write() {
        map.insert(ch_name.clone(), Arc::clone(&ch));
    }

    let ch_send = Arc::clone(&ch);
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = ch_send.send(msg).await {
                error!(channel = %ch_send.cfg.name, "custom webhook send error: {e:#}");
            }
        }
    });

    let _ = manager.register(Arc::clone(&ch) as Arc<dyn Channel>);
    tokio::spawn(async move {
        if let Err(e) = ch.run().await {
            error!("custom webhook channel error: {e:#}");
        }
    });
    info!(channel = %ch_name, "custom webhook channel started");
}

fn start_custom_websocket(
    config: &RuntimeConfig,
    ch_cfg: crate::config::schema::CustomChannelConfig,
    registry: Arc<AgentRegistry>,
    manager: &mut crate::channel::ChannelManager,
) {
    use crate::channel::custom::CustomWebSocketChannel;

    let ch_name = ch_cfg.name.clone();
    let reg = Arc::clone(&registry);
    let cfg_arc = Arc::new(config.clone());
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(64);

    let ch_name_cb = ch_name.clone();
    let on_message = Arc::new(move |sender: String, text: String, is_group: bool| {
        let reg = Arc::clone(&reg);
        let cfg = Arc::clone(&cfg_arc);
        let tx = out_tx.clone();
        let ch_name = ch_name_cb.clone();
        tokio::spawn(async move {
            let handle = match reg.route(&ch_name) {
                Ok(h) => h,
                Err(e) => {
                    error!(channel = %ch_name, "route error: {e:#}");
                    return;
                }
            };
            let dm_scope = default_dm_scope(&cfg);
            let session_key = derive_session_key(&SessionKeyParams {
                agent_id: handle.id.clone(),
                kind: if is_group {
                    MessageKind::GroupMessage {
                        group_id: sender.clone(),
                        thread_id: None,
                    }
                } else {
                    MessageKind::DirectMessage { account_id: None }
                },
                channel: ch_name.clone(),
                peer_id: sender.clone(),
                dm_scope,
            });
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let msg = AgentMessage {
                session_key,
                text,
                channel: ch_name.clone(),
                peer_id: sender.clone(),
                chat_id: sender.clone(),
                reply_tx,
                extra_tools: vec![],
                images: vec![],
                files: vec![],
            };
            if handle.tx.send(msg).await.is_err() {
                return;
            }
            if let Ok(Ok(r)) = tokio::time::timeout(Duration::from_secs(120), reply_rx).await {
                let pending = r.pending_analysis;
                if !r.is_empty {
                    let _ = tx
                        .send(OutboundMessage {
                            target_id: sender.clone(),
                            is_group,
                            text: r.text,
                            reply_to: None,
                            images: r.images,
                            files: r.files,
                            channel: None,                        })
                        .await;
                }
                if let Some(analysis) = pending {
                    handle_pending_analysis(
                        analysis,
                        Arc::clone(&handle),
                        &tx,
                        sender,
                        is_group,
                        &cfg,
                    )
                    .await;
                }
            }
        });
    });

    let ch = Arc::new(CustomWebSocketChannel::new(ch_cfg, on_message));

    let ch_send = Arc::clone(&ch);
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = ch_send.send(msg).await {
                error!(channel = %ch_send.cfg.name, "custom WS send error: {e:#}");
            }
        }
    });

    let _ = manager.register(Arc::clone(&ch) as Arc<dyn Channel>);
    tokio::spawn(async move {
        if let Err(e) = ch.run().await {
            error!("custom WS channel error: {e:#}");
        }
    });
    info!(channel = %ch_name, "custom websocket channel started");
}

// ---------------------------------------------------------------------------
// Pending file analysis helper
// ---------------------------------------------------------------------------

/// Process a pending file analysis: send the analysis text to the agent for
/// LLM processing and deliver the result (or timeout/error message) as a
/// follow-up outbound message.
async fn handle_pending_analysis(
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
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let local_cfg = search_cfg.and_then(|c| c.local.as_ref());

    // Determine model repo.
    let repo = local_cfg
        .and_then(|c| c.model_repo.as_deref())
        .unwrap_or("BAAI/bge-small-zh-v1.5");

    // Determine base URL: config override > locale auto-detect.
    let base_url = if let Some(url) = local_cfg.and_then(|c| c.model_download_url.as_deref()) {
        url.trim_end_matches('/').to_owned()
    } else {
        // Auto-detect: check LANG/LC_ALL for zh → use mirror.
        let is_zh = std::env::var("LANG")
            .or_else(|_| std::env::var("LC_ALL"))
            .unwrap_or_default()
            .to_lowercase()
            .contains("zh");
        let host = if is_zh {
            "https://hf-mirror.com"
        } else {
            "https://huggingface.co"
        };
        format!("{host}/{repo}/resolve/main")
    };

    let files = ["config.json", "model.safetensors", "tokenizer.json"];

    info!("downloading BGE embedding model from {base_url} ...");
    std::fs::create_dir_all(target_dir)?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    for filename in &files {
        let url = format!("{base_url}/{filename}");
        let dest = target_dir.join(filename);

        if dest.exists() {
            debug!("{filename} already exists, skipping");
            continue;
        }

        info!("  downloading {filename} ...");
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("failed to download {filename}: {e}"))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "failed to download {filename}: HTTP {}",
                response.status()
            );
        }

        let bytes = response.bytes().await?;
        let mut file = tokio::fs::File::create(&dest).await?;
        file.write_all(&bytes).await?;
        info!("  {filename} downloaded ({} bytes)", bytes.len());
    }

    info!("BGE model downloaded to {}", target_dir.display());
    Ok(())
}
