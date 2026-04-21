//! Integration tests: start the HTTP server and verify basic endpoints.
//!
//! These tests run against the real Axum server with a minimal AppState
//! (empty agent registry, temp-dir store, no auth token).

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Bind :0 to get a free port, drop the listener, return the SocketAddr.
/// There is a small TOCTOU window but this is acceptable for tests.
fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    listener.local_addr().expect("local_addr")
}

fn minimal_config(port: u16) -> RuntimeConfig {
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

async fn start_server(addr: SocketAddr) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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

    // Leak the tempdir so the store stays valid for the lifetime of the server.
    std::mem::forget(data_dir);

    tokio::spawn(async move {
        serve(state, addr).await.expect("serve");
    });

    // Wait until the server is accepting connections.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_returns_200() {
    let addr = free_addr();
    start_server(addr).await;

    let resp = reqwest::get(format!("http://{addr}/api/v1/health"))
        .await
        .expect("GET /api/v1/health");

    assert_eq!(resp.status(), 200, "health endpoint should return 200");
}

#[tokio::test]
async fn agents_list_returns_empty_json() {
    let addr = free_addr();
    start_server(addr).await;

    let resp = reqwest::get(format!("http://{addr}/api/v1/agents"))
        .await
        .expect("GET /api/v1/agents");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("JSON body");
    // No agents configured — expect an empty array or object.
    assert!(
        body.is_array() || body.is_object(),
        "agents response should be JSON array or object, got: {body}"
    );
}

#[tokio::test]
async fn auth_token_gates_non_health_endpoints() {
    let addr = free_addr();

    // Start server WITH an auth token.
    let mut config = minimal_config(addr.port());
    config.gateway.auth_token = Some("test-secret".to_owned());
    let config = Arc::new(config);
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
    std::mem::forget(data_dir);
    tokio::spawn(async move { serve(state, addr).await.expect("serve") });
    // wait
    for _ in 0..50 {
        if reqwest::get(format!("http://{addr}/api/v1/health"))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Health is always open.
    let r = reqwest::get(format!("http://{addr}/api/v1/health"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // /agents without token → 401.
    let r = reqwest::get(format!("http://{addr}/api/v1/agents"))
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "missing token should be 401");

    // /agents with correct token → 200.
    let client = reqwest::Client::new();
    let r = client
        .get(format!("http://{addr}/api/v1/agents"))
        .header("Authorization", "Bearer test-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "correct token should be 200");
}
