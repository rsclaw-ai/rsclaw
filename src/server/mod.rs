// TODO: split into sub-modules (routes, handlers, middleware)
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
//!   POST   /v1/files                        upload a file (multipart)
//!   GET    /v1/files                        list uploaded files
//!   GET    /v1/files/:id                    retrieve file metadata
//!   GET    /v1/files/:id/content            download file content
//!   DELETE /v1/files/:id                    delete a file
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
    extract::Multipart,
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
// Timing-safe token comparison
// ---------------------------------------------------------------------------

/// Compare two strings in constant time to prevent timing side-channel attacks.
/// Note: length difference is still detectable via timing (early return), but for
/// auth tokens of known format this is acceptable. The byte comparison itself
/// does not short-circuit.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

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
    /// Broadcast channel to notify CronRunner to reload jobs from file.
    pub cron_reload: broadcast::Sender<()>,
    /// Notification sender — routes OutboundMessage to the correct channel.
    pub notification_tx: broadcast::Sender<crate::channel::OutboundMessage>,
    /// WASM plugins for direct tool execution via API.
    pub wasm_plugins: Arc<Vec<crate::plugin::WasmPlugin>>,
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
        .route("/cron", get(cron_list).post(cron_create))
        .route("/cron/reload", post(cron_reload))
        .route("/cron/{id}", get(cron_get).put(cron_update).delete(cron_delete))
        .route("/cron/{id}/trigger", post(cron_trigger))
        .route("/cron/{id}/history", get(cron_history))
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
        .route("/a2a", post(crate::a2a::server::a2a_rpc_handler))
        .route("/tools/execute", post(execute_tool));

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
        // OpenAI Files API — file upload/management for doubao and other providers.
        .route("/v1/files", post(upload_file).get(list_files))
        .route("/v1/files/{file_id}", get(get_file_meta).delete(delete_file))
        .route("/v1/files/{file_id}/content", get(get_file_content))
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
    // Health, agent card discovery, WS, and internal reload endpoints are always open
    // (WS performs its own handshake-level auth).
    let path = request.uri().path();
    if path == "/"
        || path == "/api/v1/health"
        || path == "/.well-known/agent.json"
        || path == "/ws"
        || path == "/gateway-ws"
        || path.starts_with("/hooks/")
        || path == "/api/v1/cron/reload"
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
        Some(token) if constant_time_eq(token, &expected) => next.run(request).await,
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

    // Extract [file:path] references from user text.
    let (text, file_images, file_files) = crate::agent::registry::extract_file_refs(&req.text);

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text,
        channel: req.channel.unwrap_or_else(|| "api".to_string()),
        peer_id: req.peer_id.unwrap_or_else(|| "api-client".to_string()),
        chat_id: String::new(),
        reply_tx,
        extra_tools: vec![],
        images: file_images,
        files: file_files,
    };

    if handle.tx.send(msg).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "agent inbox closed"})),
        )
            .into_response();
    }

    // Default 600s (10 min) to match channel handlers; agent tool chains can be lengthy.
    let timeout_secs = state.config.raw.agents.as_ref()
        .and_then(|a| a.defaults.as_ref())
        .and_then(|d| d.timeout_seconds)
        .unwrap_or(600) as u64;
    let reply = match tokio::time::timeout(Duration::from_secs(timeout_secs), reply_rx).await {
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
    if id == "main" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "cannot delete the main agent" })),
        ).into_response();
    }
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

/// Execute a tool directly via HTTP — for debugging and testing.
/// POST /api/v1/tools/execute
/// Body: {"tool": "web_browser", "args": {"action": "open", "url": "..."}}
///   or: {"tool": "jimeng.txt2img", "args": {"prompt": "..."}}
async fn execute_tool(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let tool_name = body.get("tool").and_then(|v| v.as_str()).unwrap_or("");
    let args = body.get("args").cloned().unwrap_or(serde_json::json!({}));

    if tool_name.is_empty() {
        return Json(serde_json::json!({"error": "tool name required"}));
    }

    // Check WASM plugins
    if let Some((plugin_name, tool_inner)) = tool_name.split_once('.') {
        for wp in state.wasm_plugins.iter() {
            if wp.name == plugin_name {
                match wp.call_tool(tool_inner, args.clone()).await {
                    Ok(result) => return Json(serde_json::json!({"ok": true, "result": result})),
                    Err(e) => return Json(serde_json::json!({"ok": false, "error": format!("{e:#}")})),
                }
            }
        }
        return Json(serde_json::json!({"error": format!("plugin '{}' not found", plugin_name)}));
    }

    Json(serde_json::json!({"error": "use 'plugin.tool' format, e.g. 'jimeng.txt2img'"}))
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let uptime_secs = state.started_at.elapsed().as_secs();
    let hours = uptime_secs / 3600;
    let mins = (uptime_secs % 3600) / 60;
    let secs = uptime_secs % 60;
    let port = state.live.gateway.read().await.port;
    Json(serde_json::json!({
        "status": "ok",
        "version": option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"),
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
        "version": option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"),
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

async fn cron_reload(State(state): State<AppState>) -> impl IntoResponse {
    match state.cron_reload.send(()) {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"reloaded": true}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("cron reload error: {}", e)})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Cron CRUD API
// ---------------------------------------------------------------------------

/// Helper: resolve the cron.json5 path.
fn cron_jobs_path() -> std::path::PathBuf {
    crate::cron::resolve_cron_store_path()
}

/// Helper: load jobs from cron.json5 (json5 parser for comment support).
async fn cron_load_jobs() -> Vec<serde_json::Value> {
    let path = cron_jobs_path();
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let parsed: serde_json::Value = json5::from_str(&raw)
        .or_else(|_| serde_json::from_str(&raw))
        .unwrap_or_default();
    if let Some(jobs) = parsed.get("jobs").and_then(|v| v.as_array()) {
        return jobs.clone();
    }
    if let Some(arr) = parsed.as_array() {
        return arr.clone();
    }
    Vec::new()
}

/// Helper: save jobs to file and notify CronRunner to reload.
async fn cron_save_and_reload(
    jobs: &[serde_json::Value],
    reload_tx: &broadcast::Sender<()>,
) -> Result<(), String> {
    let path = cron_jobs_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create cron dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(jobs).map_err(|e| format!("serialize: {e}"))?;
    tokio::fs::write(&path, json)
        .await
        .map_err(|e| format!("write jobs.json: {e}"))?;
    let _ = reload_tx.send(());
    Ok(())
}

/// GET /api/v1/cron — list all cron jobs.
async fn cron_list() -> impl IntoResponse {
    let jobs = cron_load_jobs().await;
    Json(serde_json::json!({"jobs": jobs}))
}

/// GET /api/v1/cron/:id — get a single cron job.
async fn cron_get(Path(id): Path<String>) -> Response {
    let jobs = cron_load_jobs().await;
    match jobs.iter().find(|j| j["id"].as_str() == Some(&id)) {
        Some(job) => (StatusCode::OK, Json(job.clone())).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "job not found"})),
        )
            .into_response(),
    }
}

/// POST /api/v1/cron — create a new cron job.
async fn cron_create(
    State(state): State<AppState>,
    Json(mut body): Json<serde_json::Value>,
) -> Response {
    let id = body["id"]
        .as_str()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    body["id"] = serde_json::json!(id);
    if body.get("enabled").is_none() {
        body["enabled"] = serde_json::json!(true);
    }
    if body.get("agent_id").is_none() && body.get("agentId").is_none() {
        body["agent_id"] = serde_json::json!("main");
    }
    // Normalize schedule: accept both flat string and nested object
    if let Some(sched) = body.get("schedule").and_then(|s| s.as_str()).map(|s| s.to_owned()) {
        let tz = body.get("timezone").and_then(|t| t.as_str()).map(|t| t.to_owned());
        if let Some(tz) = tz {
            body["schedule"] = serde_json::json!({"kind": "cron", "expr": sched, "tz": tz});
        } else {
            body["schedule"] = serde_json::json!(sched);
        }
        // Remove timezone since it's now in the schedule object
        if let Some(obj) = body.as_object_mut() {
            obj.remove("timezone");
        }
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    body["created_at_ms"] = serde_json::json!(now_ms);
    body["updated_at_ms"] = serde_json::json!(now_ms);

    let mut jobs = cron_load_jobs().await;
    // Prevent duplicate IDs
    if jobs.iter().any(|j| j["id"].as_str() == Some(&id)) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "job with this id already exists"})),
        )
            .into_response();
    }
    jobs.push(body.clone());

    match cron_save_and_reload(&jobs, &state.cron_reload).await {
        Ok(()) => (StatusCode::CREATED, Json(body)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// PUT /api/v1/cron/:id — update (patch) a cron job.
async fn cron_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let mut jobs = cron_load_jobs().await;
    let idx = match jobs.iter().position(|j| j["id"].as_str() == Some(&id)) {
        Some(i) => i,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "job not found"})),
            )
                .into_response()
        }
    };

    // Merge fields from body into existing job
    if let Some(existing) = jobs[idx].as_object_mut() {
        if let Some(patch) = body.as_object() {
            for (k, v) in patch {
                // Normalize schedule string + timezone
                if k == "schedule" {
                    if let Some(sched) = v.as_str() {
                        let tz = patch
                            .get("timezone")
                            .and_then(|t| t.as_str())
                            .or_else(|| existing.get("schedule").and_then(|s| s["tz"].as_str()));
                        if let Some(tz) = tz {
                            existing.insert(
                                k.clone(),
                                serde_json::json!({"kind": "cron", "expr": sched, "tz": tz}),
                            );
                        } else {
                            existing.insert(k.clone(), serde_json::json!(sched));
                        }
                        continue;
                    }
                }
                if k == "timezone" {
                    continue; // handled with schedule
                }
                existing.insert(k.clone(), v.clone());
            }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            existing.insert("updated_at_ms".to_owned(), serde_json::json!(now_ms));
        }
    }

    let updated = jobs[idx].clone();
    match cron_save_and_reload(&jobs, &state.cron_reload).await {
        Ok(()) => (StatusCode::OK, Json(updated)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// DELETE /api/v1/cron/:id — delete a cron job.
async fn cron_delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let mut jobs = cron_load_jobs().await;
    let before = jobs.len();
    jobs.retain(|j| j["id"].as_str() != Some(&id));
    if jobs.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "job not found"})),
        )
            .into_response();
    }

    match cron_save_and_reload(&jobs, &state.cron_reload).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"deleted": true}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// POST /api/v1/cron/:id/trigger — manually trigger a cron job.
async fn cron_trigger(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let jobs = cron_load_jobs().await;
    let job = match jobs.iter().find(|j| j["id"].as_str() == Some(&id)) {
        Some(j) => j,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "job not found"})),
            )
                .into_response()
        }
    };

    let message = job["message"]
        .as_str()
        .or_else(|| job["payload"]["text"].as_str())
        .unwrap_or("")
        .to_owned();
    let agent_id = job["agent_id"]
        .as_str()
        .or_else(|| job["agentId"].as_str())
        .unwrap_or("main");

    // Send message to the agent via registry.
    // After the agent replies, deliver the result through the job's delivery channel.
    if let Ok(handle) = state.agents.get(agent_id) {
        let session_key = format!("cron:{}", id);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = crate::agent::AgentMessage {
            session_key,
            text: message,
            channel: "cron".to_string(),
            peer_id: format!("cron:{id}"),
            chat_id: String::new(),
            reply_tx,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
        };
        if handle.tx.send(msg).await.is_ok() {
            // Deliver agent reply through the job's delivery config.
            let delivery_channel = job["delivery"]["channel"].as_str().map(|s| s.to_owned());
            let delivery_to = job["delivery"]["to"].as_str().map(|s| s.to_owned());
            let ntx = state.notification_tx.clone();
            let job_id = id.clone();
            tokio::spawn(async move {
                if let Ok(reply) = reply_rx.await {
                    if !reply.text.is_empty() {
                        if let (Some(ch), Some(to)) = (delivery_channel, delivery_to) {
                            let _ = ntx.send(crate::channel::OutboundMessage {
                                target_id: to,
                                is_group: false,
                                text: reply.text,
                                reply_to: None,
                                images: reply.images.clone(),
                                files: reply.files.clone(),
                                channel: Some(ch),
                            });
                            tracing::info!(job_id = %job_id, "cron trigger: delivered reply to channel");
                        }
                    }
                }
            });
            return (
                StatusCode::OK,
                Json(serde_json::json!({"triggered": true, "job_id": id})),
            )
                .into_response();
        }
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "failed to send to agent"})),
    )
        .into_response()
}

/// GET /api/v1/cron/:id/history — get run history for a cron job.
async fn cron_history(Path(id): Path<String>) -> impl IntoResponse {
    // Read run log from data dir
    let log_dir = crate::config::loader::base_dir()
        .join("var")
        .join("data")
        .join("cron");
    let log_file = log_dir.join(format!("{id}.log.json"));
    let entries: Vec<serde_json::Value> = match tokio::fs::read_to_string(&log_file).await {
        Ok(raw) => {
            // File may contain one JSON object per line (JSONL)
            raw.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }
        Err(_) => Vec::new(),
    };
    Json(serde_json::json!({"job_id": id, "runs": entries}))
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
    let enforcers: Vec<(String, Arc<crate::channel::DmPolicyEnforcer>)> = match state
        .dm_enforcers
        .read()
    {
        Ok(guard) => guard.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal lock error"})),
            )
                .into_response();
        }
    };

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
    let enforcers: Vec<(String, Arc<crate::channel::DmPolicyEnforcer>)> = match state
        .dm_enforcers
        .read()
    {
        Ok(guard) => guard.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal lock error"})),
            )
                .into_response();
        }
    };

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
    let enforcers: Vec<(String, Arc<crate::channel::DmPolicyEnforcer>)> = match state
        .dm_enforcers
        .read()
    {
        Ok(guard) => guard.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect(),
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal lock error"})),
            )
                .into_response();
        }
    };

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
    // TODO: add full schema validation before saving (beyond JSON5 parse check).
    let backup = config_path.with_extension("json5.bak");
    if let Err(e) = std::fs::copy(&config_path, &backup) {
        tracing::warn!(error = %e, "failed to create config backup before save");
    }

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
        Ok(messages) => {
            // Filter out compaction summary messages — internal only.
            let visible: Vec<_> = messages
                .into_iter()
                .filter(|v| !is_compaction_message(v))
                .collect();
            Json(serde_json::json!({"messages": visible})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// is_compaction_message moved to crate::agent::compaction::is_compaction_message
use crate::agent::compaction::is_compaction_message;

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

    // Extract [file:path] references from user text.
    let (text, file_images, file_files) = crate::agent::registry::extract_file_refs(&text);

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
        images: file_images,
        files: file_files,
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
    api_type: Option<String>,
}

/// Resolve URL and build an authenticated request for provider model listing.
/// Shared logic between test_provider and list_provider_models.
fn build_provider_models_request(
    client: &reqwest::Client,
    req: &TestProviderRequest,
) -> Result<reqwest::RequestBuilder, String> {
    use crate::provider::defaults as prov_defaults;

    // For custom/codingplan providers, resolve auth/URL based on api_type
    let is_custom_like = req.provider == "custom" || req.provider == "codingplan";
    let effective_type = if is_custom_like {
        req.api_type.as_deref().unwrap_or("openai")
    } else {
        req.provider.as_str()
    };

    let (default_url, default_auth) = if is_custom_like {
        // custom/codingplan: no default URL, auth based on api_type
        let at = req.api_type.as_deref().unwrap_or("openai");
        let (url, auth) = prov_defaults::resolve_base_url(at);
        (url, auth)
    } else {
        prov_defaults::resolve_base_url(&req.provider)
    };

    // Resolve base URL
    let base_url = if let Some(ref explicit) = req.base_url {
        if !explicit.is_empty() { explicit.trim_end_matches('/').to_owned() }
        else if !default_url.is_empty() { default_url }
        else { return Err("no base URL provided".to_owned()); }
    } else if !default_url.is_empty() {
        default_url
    } else {
        return Err("unknown provider".to_owned());
    };

    // Determine auth style — custom/codingplan provider uses api_type, others use provider default
    let auth_style = if is_custom_like {
        match effective_type {
            "anthropic" => "x-api-key",
            "gemini" => "gemini-key",
            "ollama" => "none",
            _ => if req.api_key.is_empty() { "none" } else { "bearer" },
        }
    } else if effective_type == "gemini" {
        "gemini-key"
    } else {
        default_auth
    };

    // Build models URL — Gemini needs ?key= query param
    let is_ollama = effective_type == "ollama";
    let is_gemini = effective_type == "gemini";
    let url = if is_ollama {
        prov_defaults::models_url("ollama", &base_url)
    } else if is_gemini {
        let trimmed = base_url.trim_end_matches('/');
        format!("{trimmed}/models?key={}", req.api_key)
    } else {
        prov_defaults::models_url(&req.provider, &base_url)
    };

    let mut request = client.get(&url);
    match auth_style {
        "bearer" => { request = request.header("Authorization", format!("Bearer {}", req.api_key)); }
        "x-api-key" => {
            request = request.header("x-api-key", &req.api_key);
            request = request.header("anthropic-version", "2023-06-01");
        }
        _ => {} // "none" or "gemini-key" (already in URL)
    }

    Ok(request)
}

/// Extract model IDs from different provider response formats.
fn extract_model_ids(body: &serde_json::Value) -> Vec<String> {
    if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
        // OpenAI/Anthropic format: { data: [{ id: "..." }] }
        data.iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_owned()))
            .collect()
    } else if let Some(models) = body.get("models").and_then(|m| m.as_array()) {
        // Ollama / Gemini format: { models: [{ name: "..." }] }
        models.iter()
            .filter_map(|m| {
                m.get("name").or_else(|| m.get("id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.strip_prefix("models/").unwrap_or(s).to_owned())
            })
            .collect()
    } else {
        vec![]
    }
}

/// POST /api/v1/providers/test - validate an API key against a provider
async fn test_provider(Json(req): Json<TestProviderRequest>) -> Response {
    // Minimax doesn't support /models — return built-in list
    if req.provider == "minimax" {
        return Json(serde_json::json!({"ok": true, "status": 200})).into_response();
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();

    let request = match build_provider_models_request(&client, &req) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e}))).into_response(),
    };

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
                "error": if status == 401 || status == 403 { "Invalid API key" } else { "Request failed" },
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
    // Minimax doesn't support /models — return built-in list
    if req.provider == "minimax" {
        return Json(serde_json::json!({"models": ["MiniMax-M2.7","MiniMax-M2.7-highspeed","MiniMax-M2.5","MiniMax-M2.5-highspeed","MiniMax-M2.1","MiniMax-M2.1-highspeed","MiniMax-M2"]})).into_response();
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();

    let request = match build_provider_models_request(&client, &req) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"models": [], "error": e}))).into_response(),
    };

    match request.send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let models = extract_model_ids(&body);
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
            static ANSI_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                regex::Regex::new(r"\x1b\[[0-9;]*m").expect("ansi escape regex")
            });
            for line in stdout.lines() {
                let clean = ANSI_RE.replace_all(line, "");
                let clean = clean.trim();
                if let Some(msg) = clean.strip_prefix("[ok]") {
                    checks.push(serde_json::json!({"status": "ok", "message": msg.trim()}));
                } else if let Some(msg) = clean.strip_prefix("[warn]") {
                    checks.push(serde_json::json!({"status": "warn", "message": msg.trim()}));
                } else if let Some(msg) = clean.strip_prefix("[error]").or_else(|| clean.strip_prefix("[err]")) {
                    checks.push(serde_json::json!({"status": "error", "message": msg.trim()}));
                } else if let Some(msg) = clean.strip_prefix("[fixed]") {
                    checks.push(serde_json::json!({"status": "fixed", "message": msg.trim()}));
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
    static ANSI_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\x1b\[[0-9;]*m").expect("ansi escape regex")
    });

    let lines: Vec<&str> = content.lines().rev().take(limit).collect();
    let mut logs: Vec<serde_json::Value> = Vec::new();

    for line in lines.into_iter().rev() {
        let clean = ANSI_RE.replace_all(line, "");
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
            if let Some((before_z, rest)) = clean.split_once("Z ") {
                ts = &clean[..before_z.len() + 1]; // includes 'Z'
                let rest = rest.trim();
                // Extract level
                for lvl in &["ERROR", "WARN", "INFO", "DEBUG", "TRACE"] {
                    if let Some(after_lvl) = rest.strip_prefix(lvl) {
                        level = lvl;
                        msg = after_lvl.trim();
                        // Strip module prefix "rsclaw::xxx:"
                        if let Some((_, after_colon)) = msg.split_once(": ") {
                            msg = after_colon;
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

// ---------------------------------------------------------------------------
// OpenAI Files API
// ---------------------------------------------------------------------------

/// Maximum file upload size (100 MB). TODO: make configurable via gateway.max_upload_size.
const MAX_UPLOAD_SIZE: usize = 100 * 1024 * 1024;

/// Validate a file_id to prevent path traversal attacks.
fn validate_file_id(file_id: &str) -> Result<(), Response> {
    if !file_id.starts_with("file-") || file_id.contains('/') || file_id.contains('\\') || file_id.contains("..") {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid file_id"}))).into_response());
    }
    Ok(())
}

/// Directory where uploaded files are stored.
fn files_dir() -> std::path::PathBuf {
    dirs_next::home_dir()
        .unwrap_or_default()
        .join(".rsclaw/var/data/files")
}

/// File metadata stored alongside each uploaded file.
#[derive(Debug, Serialize, Deserialize)]
struct FileObject {
    id: String,
    object: String,
    bytes: u64,
    created_at: u64,
    filename: String,
    purpose: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

/// Generate a unique file ID: `file-{timestamp_hex}{random_hex}`.
fn generate_file_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let rnd: u32 = rand::random();
    format!("file-{ts:x}{rnd:08x}")
}

/// Read metadata JSON for a file ID.
fn read_file_meta_from_disk(file_id: &str) -> Option<FileObject> {
    let dir = files_dir();
    let meta_path = dir.join(format!("{file_id}.meta.json"));
    let data = std::fs::read_to_string(&meta_path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Derive Content-Type from file extension.
fn content_type_for(filename: &str) -> &'static str {
    match filename.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        Some("wav") => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Build the content URL for a file.
async fn file_content_url(state: &AppState, file_id: &str) -> String {
    let port = state.live.gateway.read().await.port;
    format!("http://localhost:{port}/v1/files/{file_id}/content")
}

/// POST /v1/files — upload a file via multipart/form-data.
async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let dir = files_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("cannot create files dir: {e}")})),
        )
            .into_response();
    }

    let mut file_data: Option<(String, Vec<u8>)> = None; // (filename, bytes)
    let mut purpose = String::from("assistants");

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                let filename = field
                    .file_name()
                    .unwrap_or("upload")
                    .to_string();
                match field.bytes().await {
                    Ok(b) => file_data = Some((filename, b.to_vec())),
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({"error": format!("failed to read file: {e}")})),
                        )
                            .into_response();
                    }
                }
            }
            "purpose" => {
                if let Ok(b) = field.bytes().await {
                    purpose = String::from_utf8_lossy(&b).to_string();
                }
            }
            _ => { /* ignore unknown fields */ }
        }
    }

    let Some((filename, data)) = file_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing 'file' field in multipart form"})),
        )
            .into_response();
    };

    // Max upload size: 100 MB (gateway.max_upload_size TODO).
    if data.len() > MAX_UPLOAD_SIZE {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(serde_json::json!({
            "error": "file too large, max 100MB"
        }))).into_response();
    }

    let file_id = generate_file_id();
    let stored_name = format!("{file_id}_{filename}");
    let file_path = dir.join(&stored_name);

    if let Err(e) = std::fs::write(&file_path, &data) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to write file: {e}")})),
        )
            .into_response();
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let url = file_content_url(&state, &file_id).await;

    let meta = FileObject {
        id: file_id.clone(),
        object: "file".to_string(),
        bytes: data.len() as u64,
        created_at: now,
        filename: filename.clone(),
        purpose,
        url: Some(url),
    };

    let meta_json = serde_json::to_string_pretty(&meta).unwrap_or_default();
    let meta_path = dir.join(format!("{file_id}.meta.json"));
    if let Err(e) = std::fs::write(&meta_path, &meta_json) {
        // Clean up the data file on metadata write failure.
        let _ = std::fs::remove_file(&file_path);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to write metadata: {e}")})),
        )
            .into_response();
    }

    info!(file_id = %meta.id, filename = %meta.filename, bytes = meta.bytes, "file uploaded");
    Json(serde_json::json!(meta)).into_response()
}

/// GET /v1/files — list all uploaded files.
async fn list_files(State(state): State<AppState>) -> impl IntoResponse {
    let dir = files_dir();
    let mut files: Vec<serde_json::Value> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".meta.json"))
            {
                if let Ok(data) = std::fs::read_to_string(&path) {
                    if let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&data) {
                        // Refresh the URL in case the port changed.
                        if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                            let url = file_content_url(&state, id).await;
                            obj["url"] = serde_json::Value::String(url);
                        }
                        files.push(obj);
                    }
                }
            }
        }
    }

    // Sort by created_at descending (newest first).
    files.sort_by(|a, b| {
        let ta = a.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0);
        let tb = b.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0);
        tb.cmp(&ta)
    });

    Json(serde_json::json!({
        "object": "list",
        "data": files,
    }))
}

/// GET /v1/files/{file_id} — retrieve file metadata.
async fn get_file_meta(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = validate_file_id(&file_id) { return e; }
    match read_file_meta_from_disk(&file_id) {
        Some(mut meta) => {
            meta.url = Some(file_content_url(&state, &file_id).await);
            Json(serde_json::json!(meta)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("file {file_id} not found")})),
        )
            .into_response(),
    }
}

/// GET /v1/files/{file_id}/content — download file content.
async fn get_file_content(Path(file_id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate_file_id(&file_id) { return e; }
    let dir = files_dir();

    // Find the data file matching this file_id prefix.
    let data_file = std::fs::read_dir(&dir)
        .ok()
        .and_then(|entries| {
            entries.flatten().find(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(&format!("{file_id}_")) && !name.ends_with(".meta.json")
            })
        })
        .map(|e| e.path());

    let Some(path) = data_file else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("file {file_id} not found")})),
        )
            .into_response();
    };

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let ct = content_type_for(filename);

    match std::fs::read(&path) {
        Ok(data) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, ct.parse().unwrap());
            (headers, data).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to read file: {e}")})),
        )
            .into_response(),
    }
}

/// DELETE /v1/files/{file_id} — delete a file.
async fn delete_file(Path(file_id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate_file_id(&file_id) { return e.into_response(); }
    let dir = files_dir();

    // Remove the metadata file.
    let meta_path = dir.join(format!("{file_id}.meta.json"));
    let meta_existed = meta_path.exists();
    let _ = std::fs::remove_file(&meta_path);

    // Remove the data file.
    let data_removed = std::fs::read_dir(&dir)
        .ok()
        .and_then(|entries| {
            entries.flatten().find(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(&format!("{file_id}_")) && !name.ends_with(".meta.json")
            })
        })
        .map(|e| {
            let _ = std::fs::remove_file(e.path());
            true
        })
        .unwrap_or(false);

    if meta_existed || data_removed {
        info!(file_id = %file_id, "file deleted");
    }

    Json(serde_json::json!({
        "id": file_id,
        "object": "file",
        "deleted": true,
    })).into_response()
}
