//! Shared test helpers for integration tests.

#![allow(dead_code, unused_imports)]

pub mod mock_channel;
pub mod mock_provider;

use std::{net::SocketAddr, sync::Arc};

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
    gateway::LiveConfig,
    server::{AppState, serve},
    store::Store,
};
use tokio::sync::broadcast;

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
    let config = Arc::new(minimal_config(addr.port()));
    let live = Arc::new(LiveConfig::new((*config).clone()));

    let data_dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(data_dir.path(), MemoryTier::Low).expect("store"));
    let agents = Arc::new(AgentRegistry::from_config(&config));
    let (event_tx, _) = broadcast::channel(16);

    let state = AppState {
        config,
        live,
        agents,
        store,
        event_bus: event_tx,
        devices: Arc::new(rsclaw::ws::DeviceStore::new(std::path::PathBuf::from(
            "/tmp/test-devices.json",
        ))),
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
    };

    // Leak tempdir — store must stay live for the lifetime of the server task.
    std::mem::forget(data_dir);

    tokio::spawn(async move {
        serve(state, addr).await.expect("serve");
    });

    // Poll until the health endpoint responds (up to 1 s).
    for _ in 0..50 {
        if reqwest::get(format!("http://{addr}/api/v1/health"))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("server did not start within 1 s");
}
