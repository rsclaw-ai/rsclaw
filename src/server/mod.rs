//! Axum HTTP gateway — OpenClaw-compatible REST API + SSE streaming.
//!
//! Endpoints:
//!   POST   /api/v1/message                  send message to agent
//!   GET    /api/v1/sessions                 list sessions
//!   GET    /api/v1/sessions/:id             get session
//!   DELETE /api/v1/sessions/:id             delete session
//!   GET    /api/v1/sessions/:id/messages    session message history
//!   POST   /api/v1/sessions/:id/clear       clear session context
//!   GET    /api/v1/agents                   list agents
//!   POST   /api/v1/agents                   create agent (requires restart)
//!   GET    /api/v1/agents/:id/status        agent status
//!   PATCH  /api/v1/agents/:id              update agent config (requires
//! restart)   DELETE /api/v1/agents/:id              remove agent (requires
//! restart)   GET    /api/v1/health                   health check
//!   GET    /api/v1/status                   gateway status
//!   POST   /api/v1/config/reload            trigger hot reload
//!   GET    /api/v1/config                   current config (redacted)
//!   GET    /api/v1/stream                   SSE — subscribe to agent output
//!   POST   /hooks/:path                     webhook ingress (see hooks module)
//!   POST   /v1/chat/completions             OpenAI-compatible chat endpoint
//!   GET    /v1/models                       OpenAI-compatible models list
//!   GET    /ws                              WebSocket gateway protocol
//! (OpenClaw WS)

use std::{convert::Infallible, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, patch, post},
};
use futures::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{debug, info, warn};

use crate::{
    agent::{AgentMessage, AgentRegistry},
    cmd::config_json::load_config_json,
    config::runtime::RuntimeConfig,
    gateway::LiveConfig,
    store::Store,
};

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    /// Static snapshot from startup — kept for backward-compatible handlers.
    pub config: Arc<RuntimeConfig>,
    /// Live handles — updated in-place on hot-reload.
    pub live: Arc<LiveConfig>,
    pub agents: Arc<AgentRegistry>,
    pub store: Arc<Store>,
    /// Broadcast channel for SSE: agent sends events here.
    pub event_bus: broadcast::Sender<AgentEvent>,
    /// Device token store for WebSocket gateway auth.
    pub devices: Arc<crate::ws::DeviceStore>,
    /// Active WebSocket connections registry.
    pub ws_conns: Arc<crate::ws::ConnRegistry>,
    /// Feishu channel handle for webhook events (set after startup).
    pub feishu: Arc<tokio::sync::OnceCell<Arc<crate::channel::feishu::FeishuChannel>>>,
    /// WeCom channel handle for webhook events (set after startup).
    pub wecom: Arc<tokio::sync::OnceCell<Arc<crate::channel::wecom::WeComChannel>>>,
    /// WhatsApp channel handle for webhook events (set after startup).
    pub whatsapp: Arc<tokio::sync::OnceCell<Arc<crate::channel::whatsapp::WhatsAppChannel>>>,
    /// LINE channel handle for webhook events (set after startup).
    pub line: Arc<tokio::sync::OnceCell<Arc<crate::channel::line::LineChannel>>>,
    /// Zalo channel handle for webhook events (set after startup).
    pub zalo: Arc<tokio::sync::OnceCell<Arc<crate::channel::zalo::ZaloChannel>>>,
    /// Gateway boot timestamp for uptime tracking.
    pub started_at: std::time::Instant,
    /// DM policy enforcers keyed by channel name (for pairing approval).
    pub dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<crate::channel::DmPolicyEnforcer>>>,
    >,
    /// Custom webhook channels keyed by name (for /hooks/{name} dispatch).
    pub custom_webhooks: Arc<
        std::sync::RwLock<
            std::collections::HashMap<String, Arc<crate::channel::custom::CustomWebhookChannel>>,
        >,
    >,
}

// AgentEvent is defined in crate::events to avoid circular deps with agent.
pub use crate::events::AgentEvent;

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub text: String,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub channel: Option<String>,
    pub peer_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SendMessageResponse {
    pub session_key: String,
    pub reply: String,
}

#[derive(Debug, Serialize)]
pub struct AgentStatusResponse {
    pub id: String,
    pub model: Option<String>,
    pub default: bool,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub version: &'static str,
    pub agents: usize,
}

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateAgentRequest {
    id: String,
    model: Option<String>,
    default: Option<bool>,
    system: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PatchAgentRequest {
    model: Option<String>,
    default: Option<bool>,
    system: Option<String>,
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/message", post(send_message))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/messages", get(get_session_messages))
        .route("/sessions/{id}/clear", post(clear_session))
        .route("/agents", get(list_agents).post(create_agent))
        .route("/agents/{id}", patch(patch_agent).delete(delete_agent))
        .route("/agents/{id}/status", get(agent_status))
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/config/reload", post(config_reload))
        .route("/config", get(get_config).put(save_config))
        .route("/channels/pair", post(channels_pair))
        .route("/channels/unpair", post(channels_unpair))
        .route("/channels/pairings", get(list_pairings))
        .route("/logs", get(get_logs))
        .route("/providers/test", post(test_provider))
        .route("/providers/models", post(list_provider_models))
        .route("/doctor", get(run_doctor))
        .route("/doctor/fix", post(run_doctor_fix))
        .route("/channels/wechat/qr-login", post(wechat_qr_start))
        .route("/channels/wechat/qr-status", post(wechat_qr_status))
        .route("/workspace/files", get(list_workspace_files))
        .route("/workspace/files/{*path}", get(read_workspace_file).put(write_workspace_file))
        .route("/stream", get(stream_sse))
        .route("/a2a", post(crate::a2a::server::a2a_rpc_handler));

    Router::new()
        .nest("/api/v1", api)
        .route("/hooks/feishu", post(feishu_webhook))
        .route("/hooks/wecom", get(wecom_verify).post(wecom_webhook))
        .route(
            "/hooks/whatsapp",
            get(whatsapp_verify).post(whatsapp_webhook),
        )
        .route("/hooks/line", post(line_webhook))
        .route("/hooks/zalo", post(zalo_webhook))
        .route("/hooks/{*path}", post(crate::hooks::handle_webhook))
        .route(
            "/.well-known/agent.json",
            get(crate::a2a::server::agent_card_handler),
        )
        // OpenAI-compatible endpoints — allow any OpenAI API client to connect.
        .route("/v1/chat/completions", post(openai_chat_completions))
        .route("/v1/models", get(openai_list_models))
        // WebSocket gateway — auth is handled inside the WS handshake.
        // OpenClaw WebUI connects on "/" (root), "/ws", or "/gateway-ws".
        .route("/ws", get(crate::ws::ws_handler))
        .route("/gateway-ws", get(crate::ws::ws_handler))
        .route("/", get(crate::ws::handshake::root_or_ws_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // SECURITY: permissive CORS is safe when gateway binds to loopback (default).
        // For public deployments, configure a firewall or switch to restrictive CORS.
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Start the HTTP server. Blocks until shutdown.
pub async fn serve(state: AppState, bind: std::net::SocketAddr) -> Result<()> {
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("gateway listening on {bind}");
    axum::serve(listener, router).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    // Health, agent card discovery, and WS endpoints are always open
    // (WS performs its own handshake-level auth).
    let path = request.uri().path();
    if path == "/"
        || path == "/api/v1/health"
        || path == "/.well-known/agent.json"
        || path == "/ws"
        || path == "/gateway-ws"
        || path.starts_with("/hooks/")
    {
        return next.run(request).await;
    }

    // If no token is configured, gateway runs open (loopback-only recommended).
    // Read from live config so auth_token rotation takes effect without restart.
    let expected = state.live.gateway.read().await.auth_token.clone();
    let Some(expected) = expected else {
        return next.run(request).await;
    };

    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if token == expected => next.run(request).await,
        _ => {
            warn!(path = %path, "auth rejected: missing or invalid Bearer token");
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "unauthorized"})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum allowed size (in bytes) for a message text field.
const MAX_MESSAGE_BYTES: usize = 64 * 1024; // 64 KB

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn send_message(
    State(state): State<AppState>,
    Json(req): Json<SendMessageRequest>,
) -> impl IntoResponse {
    debug!(agent_id = ?req.agent_id, session_key = ?req.session_key, text_len = req.text.len(), "HTTP send_message");
    if req.text.len() > MAX_MESSAGE_BYTES {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "message too large",
                "max_bytes": MAX_MESSAGE_BYTES
            })),
        )
            .into_response();
    }

    let agent_id = req.agent_id.as_deref().unwrap_or("main");
    let handle = match state
        .agents
        .get(agent_id)
        .or_else(|_| state.agents.default_agent())
    {
        Ok(h) => h,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let session_key = req
        .session_key
        .clone()
        .unwrap_or_else(|| format!("api:{}", uuid::Uuid::new_v4()));

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text: req.text,
        channel: req.channel.unwrap_or_else(|| "api".to_string()),
        peer_id: req.peer_id.unwrap_or_else(|| "api-client".to_string()),
        chat_id: String::new(),
        reply_tx,
        extra_tools: vec![],
        images: vec![],
        files: vec![],
    };

    if handle.tx.send(msg).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "agent inbox closed"})),
        )
            .into_response();
    }

    let reply = match tokio::time::timeout(Duration::from_secs(120), reply_rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "agent dropped reply channel"})),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({"error": "agent timed out"})),
            )
                .into_response();
        }
    };

    Json(SendMessageResponse {
        session_key,
        reply: reply.text,
    })
    .into_response()
}

async fn list_sessions(State(state): State<AppState>) -> impl IntoResponse {
    match state.store.db.list_sessions() {
        Ok(sessions) => Json(serde_json::json!({"sessions": sessions})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.store.db.get_session_meta(&id) {
        Ok(Some(s)) => Json(serde_json::json!(s)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.db.delete_session(&id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn list_agents(State(state): State<AppState>) -> impl IntoResponse {
    let agents: Vec<AgentStatusResponse> = state
        .agents
        .all()
        .into_iter()
        .map(|h| AgentStatusResponse {
            id: h.id.clone(),
            model: h.config.model.as_ref().and_then(|m| m.primary.clone()),
            default: h.config.default == Some(true),
        })
        .collect();
    Json(agents)
}

async fn agent_status(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.agents.get(&id) {
        Ok(h) => Json(AgentStatusResponse {
            id: h.id.clone(),
            model: h.config.model.as_ref().and_then(|m| m.primary.clone()),
            default: h.config.default == Some(true),
        })
        .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response(),
    }
}

async fn create_agent(
    State(_state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> impl IntoResponse {
    let id = req.id;
    let result: Result<(), anyhow::Error> = (|| {
        let (path, mut val) = load_config_json()?;
        if let Some(list) = val.pointer("/agents/list").and_then(|v| v.as_array())
            && list.iter().any(|a| a["id"].as_str() == Some(id.as_str()))
        {
            return Err(anyhow::anyhow!("conflict: agent '{}' already exists", id));
        }
        let mut new_agent = serde_json::json!({ "id": id });
        if let Some(m) = req.model {
            new_agent["model"] = serde_json::json!({ "primary": m });
        }
        if let Some(s) = req.system {
            new_agent["system"] = serde_json::json!(s);
        }
        if let Some(d) = req.default {
            new_agent["default"] = serde_json::json!(d);
        }
        if let Some(arr) = val
            .pointer_mut("/agents/list")
            .and_then(|v| v.as_array_mut())
        {
            arr.push(new_agent);
        } else {
            val["agents"] = serde_json::json!({ "list": [new_agent] });
        }
        std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;

        // Seed workspace directory for the new agent.
        let ws = resolve_workspace(Some(&id));
        if !ws.exists() {
            if let Err(e) = crate::agent::bootstrap::seed_workspace(&ws) {
                warn!(agent = %id, error = %e, "failed to seed workspace for new agent");
            } else {
                info!(agent = %id, path = %ws.display(), "seeded workspace for new agent");
            }
        }

        Ok(())
    })();
    match result {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "id": id, "created": true, "note": "restart gateway to activate" })),
        ).into_response(),
        Err(e) if e.to_string().starts_with("conflict:") => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": format!("agent '{}' already exists", id) })),
        ).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ).into_response(),
    }
}

async fn patch_agent(
    State(_state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PatchAgentRequest>,
) -> impl IntoResponse {
    let result: Result<(), anyhow::Error> = (|| {
        let (path, mut val) = load_config_json()?;
        let list = val
            .pointer_mut("/agents/list")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| anyhow::anyhow!("not found: agent '{}' not found", id))?;
        let agent = list
            .iter_mut()
            .find(|a| a["id"].as_str() == Some(id.as_str()))
            .ok_or_else(|| anyhow::anyhow!("not found: agent '{}' not found", id))?;
        if let Some(m) = req.model {
            agent["model"] = serde_json::json!({ "primary": m });
        }
        if let Some(s) = req.system {
            agent["system"] = serde_json::json!(s);
        }
        if let Some(d) = req.default {
            agent["default"] = serde_json::json!(d);
        }
        std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
        Ok(())
    })();
    match result {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "id": id, "updated": true, "note": "restart gateway to apply" })),
        ).into_response(),
        Err(e) if e.to_string().starts_with("not found:") => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("agent '{}' not found", id) })),
        ).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ).into_response(),
    }
}

async fn delete_agent(State(_state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let result: Result<(), anyhow::Error> = (|| {
        let (path, mut val) = load_config_json()?;
        let list = val
            .pointer_mut("/agents/list")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| anyhow::anyhow!("not found: agent '{}' not found", id))?;
        let before = list.len();
        list.retain(|a| a["id"].as_str() != Some(id.as_str()));
        if list.len() == before {
            return Err(anyhow::anyhow!("not found: agent '{}' not found", id));
        }
        std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
        Ok(())
    })();
    match result {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "id": id, "deleted": true, "note": "restart gateway to apply" })),
        ).into_response(),
        Err(e) if e.to_string().starts_with("not found:") => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("agent '{}' not found", id) })),
        ).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ).into_response(),
    }
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let uptime_secs = state.started_at.elapsed().as_secs();
    let hours = uptime_secs / 3600;
    let mins = (uptime_secs % 3600) / 60;
    let secs = uptime_secs % 60;
    let port = state.live.gateway.read().await.port;
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("RSCLAW_BUILD_VERSION"),
        "port": port,
        "uptime": format!("{:02}:{:02}:{:02}", hours, mins, secs),
    }))
}

async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let uptime_secs = state.started_at.elapsed().as_secs();
    let hours = uptime_secs / 3600;
    let mins = (uptime_secs % 3600) / 60;
    let secs = uptime_secs % 60;
    let uptime = format!("{:02}:{:02}:{:02}", hours, mins, secs);

    let port = state.live.gateway.read().await.port;

    // Collect channel info from channel runtime config.
    let channels: Vec<serde_json::Value> = {
        let ch = state.live.channel.read().await;
        let c = &ch.channels;
        let mut chs = Vec::new();
        macro_rules! check_ch {
            ($($name:ident),*) => {
                $(if c.$name.is_some() {
                    chs.push(serde_json::json!({
                        "type": stringify!($name),
                        "name": stringify!($name),
                        "status": "connected",
                    }));
                })*
            }
        }
        check_ch!(telegram, discord, slack, whatsapp, signal, feishu,
                   dingtalk, wecom, wechat, qq, line, zalo, matrix);
        chs
    };

    // Active session count: sessions with activity in the last 24h.
    let sessions = {
        let all = state.store.db.list_sessions().unwrap_or_default();
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64 - 86400;
        all.iter().filter(|key| {
            state.store.db.get_session_meta(key).ok().flatten()
                .map(|m| m.last_active > cutoff)
                .unwrap_or(false)
        }).count()
    };

    // Memory usage (RSS on supported platforms).
    let memory = {
        #[cfg(target_os = "macos")]
        {
            use std::process::Command;
            let pid = std::process::id();
            Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|kb| {
                    if kb > 1024 { format!("{} MB", kb / 1024) }
                    else { format!("{} KB", kb) }
                })
                .unwrap_or_else(|| "--".into())
        }
        #[cfg(not(target_os = "macos"))]
        { "--".to_string() }
    };

    Json(serde_json::json!({
        "version": env!("RSCLAW_BUILD_VERSION"),
        "agents": state.agents.len(),
        "port": port,
        "uptime": uptime,
        "memory": memory,
        "sessions": sessions,
        "channels": channels,
    }))
}

async fn config_reload(State(_state): State<AppState>) -> impl IntoResponse {
    // Re-load config from disk and broadcast a reload event.
    // Full hot-reload wiring is completed in the gateway startup path;
    // here we just validate the config is still parseable.
    match crate::config::load() {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"reloaded": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn channels_pair(
    State(state): State<AppState>,
    Json(req): Json<serde_json::Value>,
) -> Response {
    let code = req["code"].as_str().unwrap_or("");
    if code.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing code"})),
        )
            .into_response();
    }

    // Collect enforcers outside the lock to avoid holding it across await.
    let enforcers: Vec<(String, Arc<crate::channel::DmPolicyEnforcer>)> = state
        .dm_enforcers
        .read()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), Arc::clone(v)))
        .collect();

    for (channel, enforcer) in &enforcers {
        if let Some(peer_id) = enforcer.approve_pairing(code).await {
            crate::cmd::channels::persist_allow_from_pub(channel, &peer_id);
            return Json(serde_json::json!({
                "approved": true,
                "peerId": peer_id,
                "channel": channel,
            }))
            .into_response();
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "pairing code not found or expired"})),
    )
        .into_response()
}

async fn channels_unpair(
    State(state): State<AppState>,
    Json(req): Json<serde_json::Value>,
) -> Response {
    let channel = req["channel"].as_str().unwrap_or("");
    let peer_id = req["peerId"].as_str().unwrap_or("");
    if channel.is_empty() || peer_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing channel or peerId"})),
        )
            .into_response();
    }

    // Revoke from in-memory enforcer.
    let enforcers: Vec<(String, Arc<crate::channel::DmPolicyEnforcer>)> = state
        .dm_enforcers
        .read()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), Arc::clone(v)))
        .collect();

    let mut found = false;
    for (ch, enforcer) in &enforcers {
        if ch == channel {
            enforcer.revoke(peer_id).await;
            found = true;
            break;
        }
    }

    if found {
        Json(serde_json::json!({
            "revoked": true,
            "peerId": peer_id,
            "channel": channel,
        }))
        .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "channel not found"})),
        )
            .into_response()
    }
}

async fn list_pairings(State(state): State<AppState>) -> Response {
    let enforcers: Vec<(String, Arc<crate::channel::DmPolicyEnforcer>)> = state
        .dm_enforcers
        .read()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), Arc::clone(v)))
        .collect();

    let mut pending = Vec::new();
    let mut approved = Vec::new();

    for (channel, enforcer) in &enforcers {
        for (code, peer_id, ttl) in enforcer.list_pending().await {
            pending.push(serde_json::json!({
                "channel": channel,
                "peerId": peer_id,
                "code": code,
                "ttlSeconds": ttl,
            }));
        }
        for peer_id in enforcer.list_approved().await {
            approved.push(serde_json::json!({
                "channel": channel,
                "peerId": peer_id,
            }));
        }
    }

    Json(serde_json::json!({
        "pending": pending,
        "approved": approved,
    }))
    .into_response()
}

async fn get_config(State(_state): State<AppState>) -> Response {
    // Return the raw config file content for the UI editor.
    let config_path = crate::config::loader::detect_config_path()
        .unwrap_or_else(|| crate::config::loader::base_dir().join("rsclaw.json5"));
    match std::fs::read_to_string(&config_path) {
        Ok(content) => Json(serde_json::json!({
            "raw": content,
            "path": config_path.display().to_string(),
        })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct SaveConfigRequest {
    raw: String,
}

/// PUT /api/v1/config -- write raw config and trigger reload.
async fn save_config(
    State(_state): State<AppState>,
    Json(req): Json<SaveConfigRequest>,
) -> Response {
    let config_path = crate::config::loader::detect_config_path()
        .unwrap_or_else(|| crate::config::loader::base_dir().join("rsclaw.json5"));

    // Validate the new config parses before saving.
    if let Err(e) = json5::from_str::<serde_json::Value>(&req.raw) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("invalid config: {e}")})),
        ).into_response();
    }

    // Backup current file.
    let backup = config_path.with_extension("json5.bak");
    let _ = std::fs::copy(&config_path, &backup);

    // Write new config.
    if let Err(e) = std::fs::write(&config_path, &req.raw) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ).into_response();
    }

    Json(serde_json::json!({
        "saved": true,
        "path": config_path.display().to_string(),
    })).into_response()
}

async fn get_session_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.db.load_messages(&id) {
        Ok(messages) => Json(serde_json::json!({"messages": messages})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn clear_session(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    // Delete and re-create the session with empty messages.
    match state.store.db.delete_session(&id) {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"cleared": true}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible  /v1/chat/completions  and  /v1/models
// ---------------------------------------------------------------------------

/// OpenAI Chat Completions request (subset we need).
#[derive(Debug, Deserialize)]
struct OaiChatRequest {
    /// Ignored for routing; kept for wire compatibility.
    #[allow(dead_code)]
    model: Option<String>,
    messages: Vec<OaiMessage>,
    /// If true we'd stream; we return a single chunk for simplicity.
    #[serde(default)]
    stream: bool,
    /// Optional: route to a specific rsclaw agent by ID.
    #[serde(rename = "user")]
    user: Option<String>,
    /// Tool definitions forwarded to the agent for external dispatch.
    #[serde(default)]
    tools: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OaiMessage {
    role: String,
    content: String,
}

/// Parse OAI-format tool definitions into `ToolDef` values.
fn parse_oai_tools(tools: Option<&serde_json::Value>) -> Vec<crate::provider::ToolDef> {
    let Some(arr) = tools.and_then(|v| v.as_array()) else {
        return vec![];
    };
    arr.iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(crate::provider::ToolDef {
                name: f.get("name")?.as_str()?.to_owned(),
                description: f
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                parameters: f
                    .get("parameters")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default())),
            })
        })
        .collect()
}

async fn openai_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OaiChatRequest>,
) -> impl IntoResponse {
    // Extract text from the last user message.
    let text = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();

    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":{"message":"no user message found","type":"invalid_request_error"}})),
        ).into_response();
    }

    // Route: try `user` field as agent ID, then model field, then default.
    let agent_id_hint = req.user.as_deref().or(req.model.as_deref());
    let handle = match agent_id_hint
        .and_then(|id| state.agents.get(id).ok())
        .or_else(|| state.agents.default_agent().ok())
    {
        Some(h) => h,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error":{"message":"no agent available","type":"server_error"}})),
            ).into_response();
        }
    };

    // Session key: prefer X-Session-Key header (desktop UI), else hash history.
    let session_key = headers
        .get("x-session-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| {
            use std::{
                collections::hash_map::DefaultHasher,
                hash::{Hash, Hasher},
            };
            let mut h = DefaultHasher::new();
            for m in &req.messages {
                m.role.hash(&mut h);
                m.content.hash(&mut h);
            }
            format!("oai:{:x}", h.finish())
        });

    let peer_id = headers
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("desktop")
        .to_owned();

    let extra_tools = parse_oai_tools(req.tools.as_ref());
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text,
        channel: headers
            .get("x-channel")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("desktop")
            .to_owned(),
        peer_id,
        chat_id: String::new(),
        reply_tx,
        extra_tools,
        images: vec![],
        files: vec![],
    };

    // Subscribe to event_bus BEFORE sending message to agent,
    // so we don't miss early deltas for streaming responses.
    let event_rx = if req.stream { Some(state.event_bus.subscribe()) } else { None };

    if handle.tx.send(msg).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                serde_json::json!({"error":{"message":"agent inbox closed","type":"server_error"}}),
            ),
        )
            .into_response();
    }

    // For streaming: return SSE immediately, don't wait for reply.
    // For non-streaming: wait for full reply.
    if req.stream {
        let rx = event_rx.unwrap_or_else(|| state.event_bus.subscribe());
        let sid = session_key.clone();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let model_str = req.model.as_deref().unwrap_or("rsclaw").to_owned();
        let cid = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());

        let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
            .filter_map(move |msg| {
                let sid = sid.clone();
                let cid = cid.clone();
                let model_str = model_str.clone();
                async move {
                    let event = msg.ok()?;
                    if event.session_id != sid { return None; }
                    if event.done {
                        let stop = serde_json::json!({
                            "id": cid, "object": "chat.completion.chunk",
                            "created": now, "model": model_str,
                            "choices": [{"index":0,"delta":{},"finish_reason":"stop"}]
                        });
                        return Some(format!("data: {stop}\n\ndata: [DONE]\n\n"));
                    }
                    if event.delta.is_empty() { return None; }
                    let chunk = serde_json::json!({
                        "id": cid, "object": "chat.completion.chunk",
                        "created": now, "model": model_str,
                        "choices": [{"index":0,"delta":{"content":event.delta},"finish_reason":null}]
                    });
                    Some(format!("data: {chunk}\n\n"))
                }
            })
            .scan(false, |done, line| {
                if *done { return std::future::ready(None); }
                if line.contains("[DONE]") { *done = true; }
                std::future::ready(Some(Ok::<_, Infallible>(line)))
            });

        let mut response_headers = axum::http::HeaderMap::new();
        response_headers.insert(
            header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8".parse().expect("header value"),
        );
        response_headers.insert(
            header::CACHE_CONTROL,
            "no-cache".parse().expect("header value"),
        );
        response_headers.insert(
            "x-accel-buffering".parse::<axum::http::HeaderName>().expect("header name"),
            "no".parse().expect("header value"),
        );

        return (StatusCode::OK, response_headers, axum::body::Body::from_stream(stream)).into_response();
    }

    let timeout_secs = state.config.agents.defaults.timeout_seconds.unwrap_or(600) as u64;
    let reply = match tokio::time::timeout(Duration::from_secs(timeout_secs), reply_rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":{"message":"agent dropped reply","type":"server_error"}}))).into_response();
        }
        Err(_) => {
            return (StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({"error":{"message":"agent timed out","type":"server_error"}}))).into_response();
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let model_name = req.model.as_deref().unwrap_or("rsclaw");
    let completion_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let prompt_tokens = req
        .messages
        .iter()
        .map(|m| m.content.split_whitespace().count())
        .sum::<usize>() as u32;

    // If the agent returned an external tool_calls payload, relay it to the caller.
    if let Some(tool_calls) = reply.tool_calls {
        return Json(serde_json::json!({
            "id": completion_id,
            "object": "chat.completion",
            "created": now,
            "model": model_name,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": tool_calls
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": 0,
                "total_tokens": prompt_tokens
            }
        }))
        .into_response();
    }

    let content = reply.text;
    let completion_tokens = content.split_whitespace().count() as u32;

    // Streaming is handled above (before reply_rx await).

    Json(serde_json::json!({
        "id": completion_id,
        "object": "chat.completion",
        "created": now,
        "model": model_name,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        }
    }))
    .into_response()
}

async fn openai_list_models(State(state): State<AppState>) -> impl IntoResponse {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let models: Vec<serde_json::Value> = state
        .agents
        .all()
        .into_iter()
        .map(|h| {
            let model_id = h
                .config
                .model
                .as_ref()
                .and_then(|m| m.primary.as_deref())
                .unwrap_or(&h.id)
                .to_owned();
            serde_json::json!({
                "id": model_id,
                "object": "model",
                "created": now,
                "owned_by": "rsclaw"
            })
        })
        .collect();
    Json(serde_json::json!({
        "object": "list",
        "data": models
    }))
}

async fn feishu_webhook(State(state): State<AppState>, body: String) -> impl IntoResponse {
    let Some(feishu) = state.feishu.get() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "feishu not configured"})),
        )
            .into_response();
    };

    match feishu.handle_webhook_event(&body).await {
        Ok(Some(response)) => {
            // Challenge verification — return JSON
            (
                StatusCode::OK,
                Json(serde_json::from_str::<serde_json::Value>(&response).unwrap_or_default()),
            )
                .into_response()
        }
        Ok(None) => {
            // Event processed, no response body needed
            StatusCode::OK.into_response()
        }
        Err(e) => {
            warn!("feishu webhook error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// WeCom webhook handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[allow(dead_code)]
struct WeComVerifyParams {
    msg_signature: Option<String>,
    timestamp: Option<String>,
    nonce: Option<String>,
    echostr: Option<String>,
}

async fn wecom_verify(
    State(state): State<AppState>,
    Query(_params): Query<WeComVerifyParams>,
) -> impl IntoResponse {
    let Some(_wecom) = state.wecom.get() else {
        return (StatusCode::NOT_FOUND, "wecom not configured").into_response();
    };
    // WeCom AI Bot uses WebSocket mode; HTTP callback verification is not needed.
    (StatusCode::OK, "ws-mode").into_response()
}

async fn wecom_webhook(State(state): State<AppState>, _body: String) -> impl IntoResponse {
    let Some(_wecom) = state.wecom.get() else {
        return (StatusCode::NOT_FOUND, "wecom not configured").into_response();
    };
    // WeCom AI Bot uses WebSocket mode; HTTP webhook is not used.
    StatusCode::OK.into_response()
}

// ---------------------------------------------------------------------------
// WhatsApp webhook handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct WhatsAppVerifyParams {
    #[serde(rename = "hub.mode")]
    hub_mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    hub_verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    hub_challenge: Option<String>,
}

/// Meta webhook verification (GET /hooks/whatsapp).
async fn whatsapp_verify(Query(params): Query<WhatsAppVerifyParams>) -> impl IntoResponse {
    // Meta sends GET with hub.mode=subscribe, hub.verify_token, hub.challenge.
    // We accept any verify_token for now (operator should secure via WHATSAPP_VERIFY_TOKEN env).
    let expected = std::env::var("WHATSAPP_VERIFY_TOKEN").unwrap_or_default();
    if params.hub_mode.as_deref() == Some("subscribe")
        && (expected.is_empty() || params.hub_verify_token.as_deref() == Some(expected.as_str()))
    {
        if let Some(challenge) = params.hub_challenge {
            return (StatusCode::OK, challenge).into_response();
        }
    }
    (StatusCode::FORBIDDEN, "verification failed").into_response()
}

/// Inbound WhatsApp Cloud API webhook (POST /hooks/whatsapp).
async fn whatsapp_webhook(State(state): State<AppState>, body: String) -> impl IntoResponse {
    let Some(wa) = state.whatsapp.get() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "whatsapp not configured"})),
        )
            .into_response();
    };

    match serde_json::from_str::<crate::channel::whatsapp::WebhookPayload>(&body) {
        Ok(payload) => {
            wa.handle_webhook(&payload).await;
            StatusCode::OK.into_response()
        }
        Err(e) => {
            warn!("whatsapp webhook parse error: {e:#}");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// LINE webhook handler
// ---------------------------------------------------------------------------

/// Inbound LINE webhook (POST /hooks/line).
async fn line_webhook(State(state): State<AppState>, body: String) -> impl IntoResponse {
    let Some(line) = state.line.get() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "line not configured"})),
        )
            .into_response();
    };

    match line.handle_webhook(&body).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            warn!("line webhook error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Zalo webhook handler
// ---------------------------------------------------------------------------

/// Inbound Zalo OA webhook (POST /hooks/zalo).
async fn zalo_webhook(State(state): State<AppState>, body: String) -> impl IntoResponse {
    let Some(zalo) = state.zalo.get() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "zalo not configured"})),
        )
            .into_response();
    };

    match zalo.handle_webhook(&body).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            warn!("zalo webhook error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

async fn stream_sse(
    State(state): State<AppState>,
    Query(params): Query<StreamParams>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_bus.subscribe();
    let session_filter = params.session_id;

    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |msg| {
        let session_filter = session_filter.clone();
        async move {
            let event = msg.ok()?;
            if session_filter
                .as_ref()
                .is_some_and(|id| &event.session_id != id)
            {
                return None;
            }
            let data = serde_json::to_string(&event).ok()?;
            Some(Ok(Event::default().data(data)))
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

// ---------------------------------------------------------------------------
// Provider test + model listing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TestProviderRequest {
    provider: String,
    api_key: String,
    base_url: Option<String>,
}

/// POST /api/v1/providers/test - validate an API key against a provider
async fn test_provider(Json(req): Json<TestProviderRequest>) -> Response {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();

    // Resolve base URL from defaults.toml → hardcoded fallback.
    use crate::provider::defaults as prov_defaults;
    let (base_url, auth_style) = {
        let (default_url, default_auth) = prov_defaults::resolve_base_url(&req.provider);
        match req.provider.as_str() {
            "ollama" | "custom" => {
                let fallback = if default_url.is_empty() { "http://localhost:8080" } else { &default_url };
                let base = req.base_url.as_deref().unwrap_or(fallback);
                let auth = if req.provider == "custom" && !req.api_key.is_empty() { "bearer" } else { default_auth };
                (base.trim_end_matches('/').to_owned(), auth)
            }
            _ if default_url.is_empty() => {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "unknown provider"}))).into_response();
            }
            _ => {
                let base = req.base_url.as_deref().map(|u| u.trim_end_matches('/')).unwrap_or(&default_url);
                (base.to_owned(), default_auth)
            }
        }
    };

    let url = prov_defaults::models_url(&req.provider, &base_url);
    let mut request = client.get(&url);
    match auth_style {
        "bearer" => { request = request.header("Authorization", format!("Bearer {}", req.api_key)); }
        "x-api-key" => {
            request = request.header("x-api-key", &req.api_key);
            request = request.header("anthropic-version", "2023-06-01");
        }
        _ => {} // no auth (ollama)
    }

    match request.send().await {
        Ok(resp) if resp.status().is_success() => {
            Json(serde_json::json!({"ok": true, "status": resp.status().as_u16()})).into_response()
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            (StatusCode::OK, Json(serde_json::json!({
                "ok": false,
                "status": status,
                "error": if status == 401 { "Invalid API key" } else { "Request failed" },
                "detail": body.chars().take(200).collect::<String>(),
            }))).into_response()
        }
        Err(e) => {
            (StatusCode::OK, Json(serde_json::json!({
                "ok": false,
                "error": e.to_string(),
            }))).into_response()
        }
    }
}

/// POST /api/v1/providers/models - list models from a provider
async fn list_provider_models(Json(req): Json<TestProviderRequest>) -> Response {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();

    // Resolve base URL from defaults.toml → hardcoded fallback.
    use crate::provider::defaults as prov_defaults;
    let (base_url, auth_style) = {
        let (default_url, default_auth) = prov_defaults::resolve_base_url(&req.provider);
        match req.provider.as_str() {
            "ollama" | "custom" => {
                let fallback = if default_url.is_empty() { "http://localhost:8080" } else { &default_url };
                let base = req.base_url.as_deref().unwrap_or(fallback);
                let auth = if req.provider == "custom" && !req.api_key.is_empty() { "bearer" } else { default_auth };
                (base.trim_end_matches('/').to_owned(), auth)
            }
            _ if default_url.is_empty() => {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "unknown provider"}))).into_response();
            }
            _ => {
                let base = req.base_url.as_deref().map(|u| u.trim_end_matches('/')).unwrap_or(&default_url);
                (base.to_owned(), default_auth)
            }
        }
    };

    let url = prov_defaults::models_url(&req.provider, &base_url);
    let mut request = client.get(&url);
    match auth_style {
        "bearer" => { request = request.header("Authorization", format!("Bearer {}", req.api_key)); }
        "x-api-key" => {
            request = request.header("x-api-key", &req.api_key);
            request = request.header("anthropic-version", "2023-06-01");
        }
        _ => {}
    }

    match request.send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            // Normalize: extract model IDs from different API formats
            let models: Vec<String> = if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
                // OpenAI/Anthropic format: { data: [{ id: "..." }] }
                data.iter().filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_owned())).collect()
            } else if let Some(models) = body.get("models").and_then(|m| m.as_array()) {
                // Ollama format: { models: [{ name: "..." }] }
                models.iter().filter_map(|m| m.get("name").and_then(|v| v.as_str()).map(|s| s.to_owned())).collect()
            } else {
                vec![]
            };
            Json(serde_json::json!({"models": models})).into_response()
        }
        Ok(resp) => {
            (StatusCode::OK, Json(serde_json::json!({"models": [], "error": format!("HTTP {}", resp.status())}))).into_response()
        }
        Err(e) => {
            (StatusCode::OK, Json(serde_json::json!({"models": [], "error": e.to_string()}))).into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// WeChat QR code login
// ---------------------------------------------------------------------------

/// POST /api/v1/channels/wechat/qr-login
/// Start WeChat QR login, returns qrcode URL and session token.
async fn wechat_qr_start() -> Response {
    let client = reqwest::Client::new();
    match crate::channel::wechat::WeChatPersonalChannel::start_qr_login(&client).await {
        Ok((qrcode_url, qrcode_token)) => Json(serde_json::json!({
            "qrcode_url": qrcode_url,
            "qrcode_token": qrcode_token,
        })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct QrStatusRequest {
    qrcode_token: String,
}

/// POST /api/v1/channels/wechat/qr-status
/// Poll WeChat QR scan status. Returns bot_token + bot_id when scanned.
async fn wechat_qr_status(Json(req): Json<QrStatusRequest>) -> Response {
    let client = reqwest::Client::new();
    match crate::channel::wechat::WeChatPersonalChannel::poll_qr_status(&client, &req.qrcode_token).await {
        Ok(Some((bot_token, bot_id))) => Json(serde_json::json!({
            "status": "ok",
            "bot_token": bot_token,
            "bot_id": bot_id,
        })).into_response(),
        Ok(None) => Json(serde_json::json!({
            "status": "waiting",
        })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Doctor (diagnostic check + auto-fix)
// ---------------------------------------------------------------------------

/// Run `rsclaw doctor` and return structured output.
async fn run_doctor() -> Response {
    run_doctor_cmd(false).await
}

/// Run `rsclaw doctor --fix` and return structured output.
async fn run_doctor_fix() -> Response {
    run_doctor_cmd(true).await
}

async fn run_doctor_cmd(fix: bool) -> Response {
    let exe = std::env::current_exe().unwrap_or_else(|_| "rsclaw".into());
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("doctor");
    if fix {
        cmd.arg("--fix");
    }
    // Propagate instance-isolation env vars.
    if let Ok(v) = std::env::var("RSCLAW_BASE_DIR") {
        cmd.env("RSCLAW_BASE_DIR", v);
    }
    cmd.env("NO_COLOR", "1"); // Strip ANSI for clean parsing.

    match cmd.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // Parse output lines into structured results.
            let mut checks: Vec<serde_json::Value> = Vec::new();
            let ansi_re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
            for line in stdout.lines() {
                let clean = ansi_re.replace_all(line, "");
                let clean = clean.trim();
                if clean.starts_with("[ok]") {
                    checks.push(serde_json::json!({
                        "status": "ok",
                        "message": clean[4..].trim(),
                    }));
                } else if clean.starts_with("[warn]") {
                    checks.push(serde_json::json!({
                        "status": "warn",
                        "message": clean[6..].trim(),
                    }));
                } else if clean.starts_with("[err]") || clean.starts_with("[error]") {
                    let msg = if clean.starts_with("[err]") { &clean[5..] } else { &clean[7..] };
                    checks.push(serde_json::json!({
                        "status": "error",
                        "message": msg.trim(),
                    }));
                } else if clean.starts_with("[fixed]") {
                    checks.push(serde_json::json!({
                        "status": "fixed",
                        "message": clean[7..].trim(),
                    }));
                }
            }
            Json(serde_json::json!({
                "success": output.status.success(),
                "checks": checks,
                "raw": stdout,
                "stderr": stderr,
            })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Real-time logs (tail gateway.log)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LogsQuery {
    limit: Option<usize>,
}

/// GET /api/v1/logs?limit=50
/// Read the last N lines from the gateway log file, parse into structured entries.
async fn get_logs(Query(q): Query<LogsQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).min(200);
    let log_path = crate::config::loader::log_file();

    let content = match std::fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(_) => return Json(serde_json::json!({ "logs": [] })).into_response(),
    };

    // Strip ANSI escape codes.
    let ansi_re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();

    let lines: Vec<&str> = content.lines().rev().take(limit).collect();
    let mut logs: Vec<serde_json::Value> = Vec::new();

    for line in lines.into_iter().rev() {
        let clean = ansi_re.replace_all(line, "");
        let clean = clean.trim();
        if clean.is_empty() {
            continue;
        }

        // Parse format: "2026-04-03T03:32:17.581318Z  INFO rsclaw::module: message"
        let mut ts = "";
        let mut level = "INFO";
        let mut msg = clean;

        if clean.len() > 30 && clean.as_bytes().get(4) == Some(&b'-') {
            // Has timestamp
            if let Some(space_pos) = clean.find("Z ") {
                ts = &clean[..space_pos + 1];
                let rest = clean[space_pos + 1..].trim();
                // Extract level
                for lvl in &["ERROR", "WARN", "INFO", "DEBUG", "TRACE"] {
                    if rest.starts_with(lvl) {
                        level = lvl;
                        msg = rest[lvl.len()..].trim();
                        // Strip module prefix "rsclaw::xxx:"
                        if let Some(colon_pos) = msg.find(": ") {
                            msg = &msg[colon_pos + 2..];
                        }
                        break;
                    }
                }
            }
        }

        // Format timestamp to local HH:MM:SS
        let short_ts = if ts.len() >= 19 {
            // Parse UTC timestamp and convert to local time.
            chrono::NaiveDateTime::parse_from_str(&ts[..19], "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|naive| {
                    let utc = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc);
                    utc.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()
                })
                .unwrap_or_else(|| ts[11..19].to_owned())
        } else {
            ts.to_owned()
        };

        logs.push(serde_json::json!({
            "ts": short_ts,
            "level": match level {
                "ERROR" => "ERROR",
                "WARN" => "WARN",
                "DEBUG" => "DEBUG",
                _ => "INFO",
            },
            "msg": msg,
        }));
    }

    Json(serde_json::json!({ "logs": logs })).into_response()
}

// ---------------------------------------------------------------------------
// Workspace file management
// ---------------------------------------------------------------------------

/// Resolve workspace directory for an agent (or default workspace).
fn resolve_workspace(agent_id: Option<&str>) -> std::path::PathBuf {
    let base = crate::config::loader::base_dir();
    match agent_id {
        Some(id) if !id.is_empty() && id != "default" && id != "main" => {
            base.join(format!("workspace-{id}"))
        }
        _ => base.join("workspace"),
    }
}

#[derive(Debug, Deserialize)]
struct WorkspaceQuery {
    agent: Option<String>,
}

/// GET /api/v1/workspace/files?agent=xxx
/// List .md files in a workspace directory.
async fn list_workspace_files(
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let ws = resolve_workspace(q.agent.as_deref());
    if !ws.exists() {
        return Json(serde_json::json!({ "files": [] })).into_response();
    }
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&ws) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    files.push(name.to_owned());
                }
            }
        }
    }
    files.sort();
    Json(serde_json::json!({ "files": files, "workspace": ws.display().to_string() })).into_response()
}

/// GET /api/v1/workspace/files/{path}?agent=xxx
/// Read a workspace file.
async fn read_workspace_file(
    Path(file_path): Path<String>,
    Query(q): Query<WorkspaceQuery>,
) -> Response {
    let ws = resolve_workspace(q.agent.as_deref());
    // Security: only allow .md files, no path traversal.
    let file_name = std::path::Path::new(&file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if file_name.is_empty() || !file_name.ends_with(".md") || file_name.contains("..") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid file path"})),
        ).into_response();
    }
    let full_path = ws.join(file_name);
    match std::fs::read_to_string(&full_path) {
        Ok(content) => Json(serde_json::json!({
            "file": file_name,
            "content": content,
        })).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "file not found"})),
        ).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct WriteFileRequest {
    content: String,
}

/// PUT /api/v1/workspace/files/{path}?agent=xxx
/// Write a workspace file.
async fn write_workspace_file(
    Path(file_path): Path<String>,
    Query(q): Query<WorkspaceQuery>,
    Json(req): Json<WriteFileRequest>,
) -> Response {
    let ws = resolve_workspace(q.agent.as_deref());
    let file_name = std::path::Path::new(&file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if file_name.is_empty() || !file_name.ends_with(".md") || file_name.contains("..") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid file path"})),
        ).into_response();
    }
    // Create workspace dir if it doesn't exist.
    if !ws.exists() {
        if let Err(e) = std::fs::create_dir_all(&ws) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            ).into_response();
        }
    }
    let full_path = ws.join(file_name);
    match std::fs::write(&full_path, &req.content) {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "file": file_name,
        })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ).into_response(),
    }
}
