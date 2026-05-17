//! Shared test helpers for integration tests.

#![allow(dead_code, unused_imports)]

pub mod mock_channel;
pub mod mock_provider;

use std::{net::SocketAddr, sync::Arc};

/// Initialize TLS crypto provider (rustls + aws-lc-rs).
/// Safe to call multiple times; only the first call installs the provider.
pub fn init_tls() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

use rsclaw::{
    MemoryTier,
    agent::AgentRegistry,
    config::{
        runtime::{
            AgentsRuntime, ChannelRuntime, ExtRuntime, GatewayRuntime, ModelRuntime, OpsRuntime,
            RuntimeConfig,
        },
        schema::{BindMode, GatewayMode, ReloadMode, SessionConfig},
    },
    events::RestartRequest,
    gateway::{LiveConfig, ShutdownCoordinator},
    server::{AppState, serve},
    store::Store,
};
use tokio::sync::broadcast;

/// Handles into the running server's AppState. Returned by
/// [`start_server_with_handles`] so tests can publish events directly into the
/// gateway's broadcast channels and inspect latched state.
pub struct ServerHandles {
    pub restart_request_tx: broadcast::Sender<RestartRequest>,
    pub pending_restart: Arc<std::sync::RwLock<Option<RestartRequest>>>,
    pub shutdown: ShutdownCoordinator,
}

/// Allocate a free TCP port by binding :0 and returning the address.
/// The listener is dropped immediately; there is a small TOCTOU window
/// that is acceptable for tests.
pub fn free_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    l.local_addr().expect("local_addr")
}

/// Build a minimal RuntimeConfig with no agents, no auth token.
pub fn minimal_config(port: u16) -> RuntimeConfig {
    RuntimeConfig {
        gateway: GatewayRuntime {
            port,
            mode: GatewayMode::Local,
            bind: BindMode::Loopback,
            bind_address: None,
            reload: ReloadMode::Hybrid,
            auth_token: None,
            auth_token_configured: false,
            auth_token_is_plaintext: false,
            allow_tailscale: false,
            channel_health_check_minutes: 5,
            channel_stale_event_threshold_minutes: 30,
            channel_max_restarts_per_hour: 10,
            user_agent: None,
            language: None,
        },
        agents: AgentsRuntime {
            defaults: Default::default(),
            list: vec![],
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

/// Spawn a minimal Axum server on `addr` and wait until it is ready.
/// The caller must ensure `addr` is not reused before this future resolves.
pub async fn start_server(addr: SocketAddr) {
    let _ = start_server_with_handles(addr).await;
}

/// Like [`start_server`], but returns handles into the running AppState so a
/// test can publish events into broadcast channels or inspect latched state.
pub async fn start_server_with_handles(addr: SocketAddr) -> ServerHandles {
    init_tls();
    let config = Arc::new(minimal_config(addr.port()));
    let live = Arc::new(LiveConfig::new((*config).clone()));

    let data_dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(data_dir.path(), MemoryTier::Low).expect("store"));
    let agents = Arc::new(AgentRegistry::from_config(&config));
    let (event_tx, _) = broadcast::channel(16);

    let restart_request_tx: broadcast::Sender<RestartRequest> = broadcast::channel(16).0;
    let pending_restart: Arc<std::sync::RwLock<Option<RestartRequest>>> =
        Arc::new(std::sync::RwLock::new(None));
    let shutdown = ShutdownCoordinator::new();

    // Per-test device-store path so tests don't share token state on disk.
    let device_path = tempfile::Builder::new()
        .prefix("rsclaw-test-devices-")
        .suffix(".json")
        .tempfile()
        .expect("device tempfile")
        .into_temp_path()
        .keep()
        .expect("keep device path");

    // computer_use plumbing — production fills these in
    // `gateway::startup::serve_with_runtime`. Tests don't drive a UI
    // dialog, so the permission store starts in non-bypass mode (every
    // request would prompt) and the broadcast channels exist purely so
    // dependent handlers don't blow up if they touch the field.
    let computer_permission = Arc::new(
        rsclaw::computer::permission::RedbPermissionStore::new(
            Arc::clone(&store.db),
            false,
        ),
    );
    let (computer_permission_tx, _) =
        broadcast::channel::<rsclaw::computer::permission::PermissionRequest>(64);
    let (computer_status_tx, _) =
        broadcast::channel::<rsclaw::computer::status::ComputerUseStatus>(256);
    let computer_runs: Arc<
        tokio::sync::RwLock<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
    > = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

    let state = AppState {
        config,
        live,
        agents,
        store,
        event_bus: event_tx,
        computer_permission,
        computer_permission_tx,
        computer_status_tx,
        computer_runs,
        devices: Arc::new(rsclaw::ws::DeviceStore::new(device_path)),
        ws_conns: Arc::new(rsclaw::ws::ConnRegistry::new()),
        feishu: Arc::new(tokio::sync::OnceCell::new()),
        wecom: Arc::new(tokio::sync::OnceCell::new()),
        whatsapp: Arc::new(tokio::sync::OnceCell::new()),
        line: Arc::new(tokio::sync::OnceCell::new()),
        zalo: Arc::new(tokio::sync::OnceCell::new()),
        started_at: std::time::Instant::now(),
        dm_enforcers: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        custom_webhooks: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        cron_reload: broadcast::channel(1).0,
        notification_tx: broadcast::channel(16).0,
        wasm_plugins: Arc::new(Vec::new()),
        plugins: Arc::new(rsclaw::plugin::PluginRegistry::default()),
        restart_request_tx: restart_request_tx.clone(),
        pending_restart: Arc::clone(&pending_restart),
        shutdown: shutdown.clone(),
        task_event_bus: rsclaw::a2a::event::TaskEventBus::new(),
        task_cancels: Arc::new(dashmap::DashMap::new()),
        suspended_tasks: Arc::new(dashmap::DashMap::new()),
        task_store: {
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = tmp.path().join("a2a-tasks.redb");
            std::mem::forget(tmp);
            Arc::new(rsclaw::a2a::store::TaskStore::open(&path).expect("a2a store"))
        },
        push_dispatcher: {
            let bus = rsclaw::a2a::event::TaskEventBus::new();
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = tmp.path().join("a2a-tasks.redb");
            std::mem::forget(tmp);
            let store = Arc::new(rsclaw::a2a::store::TaskStore::open(&path).expect("a2a store"));
            Arc::new(rsclaw::a2a::push::PushDispatcher::new(store, bus))
        },
    };

    // Leak tempdir — store must stay live for the lifetime of the server task.
    std::mem::forget(data_dir);

    tokio::spawn(async move {
        serve(state, addr).await.expect("serve");
    });

    // Poll until the health endpoint responds (up to 1 s).
    let mut ready = false;
    for _ in 0..50 {
        if reqwest::get(format!("http://{addr}/api/v1/health"))
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(ready, "server did not start within 1 s");

    ServerHandles {
        restart_request_tx,
        pending_restart,
        shutdown,
    }
}
