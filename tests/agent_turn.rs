//! Integration test: agent turn smoke-test.
//!
//! Starts an HTTP server with one echo agent (no LLM — just echoes the
//! incoming text back).  Exercises the full HTTP ↔ agent-inbox path:
//!
//!   POST /api/v1/message → AgentMessage.tx → echo task → reply_tx → JSON
//!
//! No API keys required.

use std::{net::SocketAddr, sync::Arc};

use rsclaw::{
    MemoryTier,
    agent::{AgentRegistry, AgentReply},
    config::{
        runtime::{
            AgentsRuntime, ChannelRuntime, ExtRuntime, GatewayRuntime, ModelRuntime, OpsRuntime,
            RuntimeConfig,
        },
        schema::{AgentEntry, BindMode, GatewayMode, ReloadMode, SessionConfig},
    },
    gateway::LiveConfig,
    server::{AppState, serve},
    store::Store,
};
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    listener.local_addr().expect("local_addr")
}

fn config_with_echo_agent(port: u16) -> RuntimeConfig {
    RuntimeConfig {
        gateway: GatewayRuntime {
            port,
            mode: GatewayMode::Local,
            bind: BindMode::Loopback,
            reload: ReloadMode::Hybrid,
            auth_token: None,
            auth_token_configured: false,
            auth_token_is_plaintext: false,
            allow_tailscale: false,
            channel_health_check_minutes: 5,
            channel_stale_event_threshold_minutes: 30,
            channel_max_restarts_per_hour: 10,
        },
        agents: AgentsRuntime {
            defaults: Default::default(),
            list: vec![AgentEntry {
                id: "echo".to_string(),
                default: Some(true),
                workspace: None,
                model: None,
                lane: None,
                lane_concurrency: None,
                group_chat: None,
                channels: None,
                name: None,
                agent_dir: None,
                system: None,
            }],
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

/// Start a server where every agent inbox is handled by an echo task.
async fn start_echo_server(addr: SocketAddr) {
    let config = Arc::new(config_with_echo_agent(addr.port()));
    let live = Arc::new(LiveConfig::new((*config).clone()));

    let data_dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(data_dir.path(), MemoryTier::Low).expect("store"));
    let (event_tx, _) = broadcast::channel(16);

    // Build registry with receivers so we can attach echo loops.
    let providers = std::sync::Arc::new(rsclaw::provider::registry::ProviderRegistry::new());
    let (registry, receivers) = AgentRegistry::from_config_with_receivers(&config, providers);

    // Spawn a lightweight echo task for each agent — no LLM involved.
    for (_id, mut rx) in receivers {
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let _ = msg.reply_tx.send(AgentReply {
                    text: format!("echo: {}", msg.text),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    pending_analysis: None,
                });
            }
        });
    }

    let state = AppState {
        config,
        live,
        agents: Arc::new(registry),
        store,
        event_bus: event_tx,
        devices: Arc::new(rsclaw::ws::DeviceStore::new(std::path::PathBuf::from(
            "/tmp/test-devices.json",
        ))),
        ws_conns: Arc::new(rsclaw::ws::ConnRegistry::new()),
        feishu: Arc::new(tokio::sync::OnceCell::new()),
        started_at: std::time::Instant::now(),
        dm_enforcers: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
    };

    // Leak the tempdir so the store stays valid for the server's lifetime.
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
async fn agent_turn_echo_reply() {
    let addr = free_addr();
    start_echo_server(addr).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/api/v1/message"))
        .json(&serde_json::json!({"text": "hello from test"}))
        .send()
        .await
        .expect("POST /api/v1/message");

    assert_eq!(resp.status(), 200, "send_message should return 200");

    let body: serde_json::Value = resp.json().await.expect("JSON body");
    let reply = body["reply"].as_str().expect("reply field missing");
    let session_key = body["session_key"]
        .as_str()
        .expect("session_key field missing");

    assert!(!reply.is_empty(), "reply should not be empty");
    assert!(
        reply.contains("hello from test"),
        "echo should contain the original text, got: {reply}"
    );
    assert!(!session_key.is_empty(), "session_key should be present");
}

#[tokio::test]
async fn agent_turn_explicit_session_key_preserved() {
    let addr = free_addr();
    start_echo_server(addr).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/api/v1/message"))
        .json(&serde_json::json!({
            "text": "ping",
            "session_key": "test-session-42"
        }))
        .send()
        .await
        .expect("POST /api/v1/message");

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("JSON body");
    assert_eq!(
        body["session_key"].as_str(),
        Some("test-session-42"),
        "provided session_key should be echoed back in the response"
    );
}

#[tokio::test]
async fn agent_turn_unknown_agent_id_falls_back_to_default() {
    let addr = free_addr();
    start_echo_server(addr).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/api/v1/message"))
        .json(&serde_json::json!({
            "text": "hello",
            "agent_id": "nonexistent"
        }))
        .send()
        .await
        .expect("POST /api/v1/message");

    // send_message falls back to default_agent() when the named one isn't found.
    // Our single echo agent is registered as default, so this must succeed.
    assert_eq!(resp.status(), 200, "should fall back to default agent");
}
