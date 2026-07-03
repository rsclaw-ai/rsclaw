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
//!   GET    /api/v1/defaults                 provider/channel/search catalog
//! (defaults.toml)   GET    /api/v1/stream                   SSE — subscribe to
//! agent output   POST   /hooks/:path                     webhook ingress (see
//! hooks module)   POST   /v1/chat/completions             OpenAI-compatible
//! chat endpoint   GET    /v1/models                       OpenAI-compatible
//! models list   POST   /v1/files                        upload a file
//! (multipart)   GET    /v1/files                        list uploaded files
//!   GET    /v1/files/:id                    retrieve file metadata
//!   GET    /v1/files/:id/content            download file content
//!   DELETE /v1/files/:id                    delete a file
//!   GET    /ws                              WebSocket gateway protocol
//! (OpenClaw WS)

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use std::{collections::HashMap, net::IpAddr, time::Instant};
use std::{convert::Infallible, path::PathBuf, process::Command, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, patch, post, put},
};
use futures::{Stream, StreamExt as _};
use rsclaw_agent::{AgentMessage, AgentRegistry};
use rsclaw_config::runtime::RuntimeConfig;
use rsclaw_store::Store;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{info, warn};

use crate::{cmd::config_json::load_config_json, gateway::LiveConfig, ws::types::EventFrame};

mod knowledge;

const MAX_LOCAL_MEDIA_BYTES: u64 = 10 * 1024 * 1024;

// H1: simple in-memory per-IP rate limiter with a sliding window.
// Default: 100 req / 60s per IP. Bypassed when no limit is configured (rate_rps = 0).
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<RwLock<HashMap<IpAddr, Vec<Instant>>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns true if the request should be rate-limited (rejected).
    pub async fn check(&self, ip: IpAddr, max_req: u32, window_secs: u64) -> bool {
        if max_req == 0 {
            return false; // rate limiting disabled
        }
        let now = Instant::now();
        let mut map = self.inner.write().await;
        let entries = map.entry(ip).or_default();
        let cutoff = now - Duration::from_secs(window_secs);
        entries.retain(|t| *t >= cutoff);
        if entries.len() >= max_req as usize {
            true
        } else {
            entries.push(now);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Timing-safe token comparison
// ---------------------------------------------------------------------------

/// Compare two strings in constant time to prevent timing side-channel attacks.
/// Note: length difference is still detectable via timing (early return), but
/// for auth tokens of known format this is acceptable. The byte comparison
/// itself does not short-circuit.
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
    /// Shared computer_use permission store. Drivers register pending
    /// requests via `register_pending_request`; the WS handler resolves
    /// them via `resolve_pending_request` once the user clicks a button
    /// in the Tauri permission dialog.
    pub computer_permission: Arc<rsclaw_computer::permission::RedbPermissionStore>,
    /// Broadcast channel for `PermissionRequest` events. Subscribed by
    /// the WS gateway (delivered to the desktop UI as `permission_request`
    /// frames).
    pub computer_permission_tx: broadcast::Sender<rsclaw_computer::permission::PermissionRequest>,
    /// Broadcast channel for `ComputerUseStatus` events (Started/Step/
    /// Finished). Same WS plumbing as `computer_permission_tx`; delivered
    /// to the desktop UI as `computer_use_status` frames so the live
    /// status panel can show what the GUI agent is doing right now.
    pub computer_status_tx: broadcast::Sender<rsclaw_computer::status::ComputerUseStatus>,
    /// Live registry of in-flight `computer_use` runs.
    /// Maps `run_id` -> the driver's abort flag. Inserted when
    /// `tool_vlm_drive` mints a run and removed on driver exit. The
    /// `POST /computer-use/runs/{run_id}/abort` HTTP endpoint sets the
    /// flag; the driver's loop checks it between steps and exits with
    /// `DriverOutcome::UserAbort` -> `Finished { outcome_kind: "user_abort" }`,
    /// which the overlay UI consumes to fade itself out.
    pub computer_runs: Arc<
        tokio::sync::RwLock<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
    >,
    /// Device token store for WebSocket gateway auth.
    pub devices: Arc<crate::ws::DeviceStore>,
    /// Active WebSocket connections registry.
    pub ws_conns: Arc<crate::ws::ConnRegistry>,
    /// Feishu channel handle for webhook events (set after startup).
    pub feishu: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::feishu::FeishuChannel>>>,
    /// WeCom channel handle for webhook events (set after startup).
    pub wecom: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::wecom::WeComChannel>>>,
    /// WhatsApp channel handle for webhook events (set after startup).
    pub whatsapp: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::whatsapp::WhatsAppChannel>>>,
    /// LINE channel handle for webhook events (set after startup).
    pub line: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::line::LineChannel>>>,
    /// Zalo channel handle for webhook events (set after startup).
    pub zalo: Arc<tokio::sync::OnceCell<Arc<rsclaw_channel::zalo::ZaloChannel>>>,
    /// Gateway boot timestamp for uptime tracking.
    pub started_at: std::time::Instant,
    /// DM policy enforcers keyed by channel name (for pairing approval).
    pub dm_enforcers: Arc<
        std::sync::RwLock<std::collections::HashMap<String, Arc<rsclaw_channel::DmPolicyEnforcer>>>,
    >,
    /// Custom webhook channels keyed by name (for /hooks/{name} dispatch).
    pub custom_webhooks: Arc<
        std::sync::RwLock<
            std::collections::HashMap<String, Arc<rsclaw_channel::custom::CustomWebhookChannel>>,
        >,
    >,
    /// Broadcast channel to notify CronRunner to reload jobs from file.
    pub cron_reload: broadcast::Sender<()>,
    /// Notification sender — routes OutboundMessage to the correct channel.
    pub notification_tx: broadcast::Sender<rsclaw_channel::OutboundMessage>,
    /// WASM plugins for direct tool execution via API.
    pub wasm_plugins: Arc<Vec<rsclaw_plugin::WasmPlugin>>,
    /// Shell-bridge plugin registry for direct tool execution via API.
    pub plugins: Arc<rsclaw_plugin::PluginRegistry>,
    /// Broadcast channel for restart-required events
    /// (config changed, model downloaded, etc.). Multi-source, single sink.
    pub restart_request_tx: broadcast::Sender<rsclaw_events::RestartRequest>,
    /// Latch holding the current pending restart request, if any.
    /// Read on WS handshake so late-connecting UIs see prior events.
    pub pending_restart: Arc<std::sync::RwLock<Option<rsclaw_events::RestartRequest>>>,
    /// Graceful shutdown coordinator — used by /api/v1/restart to drain
    /// in-flight HTTP requests, task queue tasks, and channel handlers
    /// before re-exec.
    pub shutdown: crate::gateway::ShutdownCoordinator,
    /// A2A v1.0 per-task event broadcast bus (fan-out for SSE subscribers
    /// and the push notification dispatcher).
    pub task_event_bus: crate::a2a::event::TaskEventBus,
    /// Cancellation tokens for in-flight A2A tasks, keyed by task_id.
    /// Inserted on SendMessage / SendStreamingMessage entry; fired by
    /// CancelTask.
    pub task_cancels: Arc<dashmap::DashMap<String, tokio_util::sync::CancellationToken>>,
    /// Tasks paused on TASK_STATE_INPUT_REQUIRED / AUTH_REQUIRED, keyed by
    /// task_id. Resumed when the client sends another SendMessage with the
    /// matching taskId.
    pub suspended_tasks: Arc<dashmap::DashMap<String, crate::a2a::event::SuspendedTask>>,
    /// Persistent A2A task / push-config store (redb).
    pub task_store: Arc<crate::a2a::store::TaskStore>,
    /// Push notification dispatcher (subscribes to event bus, signs payloads).
    pub push_dispatcher: Arc<crate::a2a::push::PushDispatcher>,
    /// Private rsclaw A2A relay hub state. Empty/idle unless
    /// `gateway.a2a.relay.mode = "hub"`.
    pub relay_hub: Arc<crate::a2a::relay::RelayHub>,
    /// User-managed RAG knowledge base (desktop `/api/v1/knowledge/*`).
    /// Collections are a tag veneer over the single KB store. `None` when
    /// the KB store failed to open — the gateway still serves everything
    /// else and the `/knowledge` routes are simply not mounted.
    pub knowledge: Option<Arc<rsclaw_kb::KnowledgeService>>,
    /// Gateway-wide agent memory store (redb), opened once at startup with an
    /// exclusive write lock. The `/api/v1/memory/*` handlers read it through
    /// here — it is a singleton, not per-agent, so it does not hang off an
    /// `AgentHandle`. `None` when memory is disabled.
    pub memory: Option<Arc<tokio::sync::Mutex<rsclaw_agent::memory::MemoryStore>>>,
    /// Shared per-model health table — read by `/api/v1/models/health`,
    /// written by every `FailoverManager` in the gateway. See
    /// `provider::health::ProviderHealthRegistry`.
    pub model_health: rsclaw_provider::health::ProviderHealthRegistry,
    /// H1: per-IP rate limiter for inbound HTTP requests.
    pub rate_limiter: Arc<RateLimiter>,
}

// AgentEvent is defined in rsclaw_events to avoid circular deps with agent.
pub use rsclaw_events::AgentEvent;

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    // Aliases let the `rsclaw agent-turn` CLI (which posts to /api/v1/agent/turn
    // with camelCase field names) share this handler. Extra agent-turn fields
    // (deliver/thinking/local/timeout/replyTo/…) are ignored.
    #[serde(alias = "message")]
    pub text: String,
    #[serde(alias = "sessionId")]
    pub session_key: Option<String>,
    #[serde(alias = "agent")]
    pub agent_id: Option<String>,
    pub channel: Option<String>,
    #[serde(alias = "to")]
    pub peer_id: Option<String>,
    #[serde(default)]
    pub stream: bool,
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
    let mut api = Router::new()
        .route("/message", post(send_message))
        // `rsclaw agent-turn` CLI alias — same handler, camelCase field aliases.
        .route("/agent/turn", post(send_message))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/messages", get(get_session_messages))
        .route("/sessions/{id}/clear", post(clear_session))
        .route("/agents", get(list_agents).post(create_agent))
        .route("/agents/{id}", patch(patch_agent).delete(delete_agent))
        .route("/agents/{id}/status", get(agent_status))
        .route("/acp/connections", get(list_acp_connections))
        .route("/message/send", post(message_send))
        .route("/message/read", get(message_read))
        .route("/message/broadcast", post(message_broadcast))
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/config/reload", post(config_reload))
        .route("/shutdown", post(http_shutdown))
        .route("/restart", post(http_restart))
        .route("/restart-dismiss", post(http_restart_dismiss))
        .route("/config", get(get_config).put(save_config))
        .route("/cron", get(cron_list).post(cron_create))
        .route("/cron/reload", post(cron_reload))
        .route("/cron/bulk_replace", put(cron_bulk_replace))
        .route(
            "/cron/{id}",
            get(cron_get).put(cron_update).delete(cron_delete),
        )
        .route("/cron/{id}/trigger", post(cron_trigger))
        .route("/cron/{id}/history", get(cron_history))
        .route("/channels/pair", post(channels_pair))
        .route("/channels/unpair", post(channels_unpair))
        .route("/channels/pairings", get(list_pairings))
        .route("/logs", get(get_logs))
        .route("/providers/test", post(test_provider))
        .route("/providers/models", post(list_provider_models))
        .route("/defaults", get(get_defaults))
        .route("/models/health", get(models_health))
        .route("/models/health/reset", post(models_health_reset))
        .route("/doctor", get(run_doctor))
        .route("/doctor/fix", post(run_doctor_fix))
        .route("/channels/wechat/qr-login", post(wechat_qr_start))
        .route("/channels/wechat/qr-status", post(wechat_qr_status))
        .route("/workspace/files", get(list_workspace_files))
        .route(
            "/workspace/files/{*path}",
            get(read_workspace_file).put(write_workspace_file),
        )
        .route("/stream", get(stream_sse))
        .route(
            "/a2a",
            // axum's stock Json<T> body limit is 2 MiB which 413s any
            // realistic file attachment. Honour gateway.a2a.maxBodyMb
            // (default 100 MiB resolved in runtime.rs). The
            // DefaultBodyLimit::max layer must wrap the route directly
            // (before the middlewares) so axum's body-aware extractors
            // see the larger limit.
            post(crate::a2a::server::a2a_dispatch)
                .layer(axum::extract::DefaultBodyLimit::max(
                    state
                        .config
                        .gateway
                        .a2a_max_body_bytes
                        .min(usize::MAX as u64) as usize,
                ))
                .layer(axum::middleware::from_fn(
                    crate::a2a::version::a2a_version_layer,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    crate::a2a::auth::a2a_auth_layer,
                )),
        )
        .route("/a2a/relay/ws", get(crate::a2a::relay::relay_ws_handler))
        .route(
            "/a2a/relay/stats",
            get(crate::a2a::relay::relay_stats_handler),
        )
        .route("/tools/execute", post(execute_tool))
        .route("/hub/catalog", get(hub_catalog))
        .route("/hub/skills", get(hub_skills))
        .route("/hub/plugins", get(hub_plugins))
        .route("/hub/tools", get(hub_tools))
        .route("/plugins/{name}/tools", get(plugin_describe))
        .route(
            "/computer-use/permissions",
            get(computer_use_permissions_list),
        )
        .route(
            "/computer-use/permissions/{agent_id}/{app}",
            delete(computer_use_permissions_revoke),
        )
        .route(
            "/computer-use/bypass",
            get(computer_use_bypass_get).put(computer_use_bypass_set),
        )
        .route(
            "/computer-use/runs/{run_id}/abort",
            post(computer_use_run_abort),
        )
        .route("/computer-use/stream", get(computer_use_stream_sse))
        .route(
            "/computer-use/permission_response",
            post(computer_use_permission_response),
        )
        .route("/git/status", get(git_status))
        .route("/git/diff", get(git_diff))
        .route("/git/log", get(git_log))
        .route("/git/commit", post(git_commit))
        .route("/git/prs", get(git_prs))
        .route("/git/prs/{number}", get(git_pr_get))
        .route("/git/prs/{number}/review", post(git_pr_review))
        // Memory management — read-only browse + stats. Each request
        // opens `MemoryStore::open_readonly` to avoid touching the
        // agent runtime's live store. Mutating endpoints (delete,
        // pin/unpin, importance bump) need shared-state coordination
        // and land in a follow-up.
        .route("/memory/docs", get(memory_list_docs).post(memory_add_doc))
        .route("/memory/stats", get(memory_stats));

    // Mount the knowledge base routes only when the store opened. A KB
    // open failure leaves `knowledge` None; the rest of the API still serves.
    if let Some(k) = state.knowledge.clone() {
        let max_doc_bytes = k.max_doc_bytes();
        api = api.nest("/knowledge", knowledge::routes(max_doc_bytes).with_state(k));
    }

    Router::new()
        .nest("/api/v1", api)
        // Convenience alias: bare `/health` resolves to the same handler
        // as `/api/v1/health`. Container orchestrators (Docker, k8s,
        // generic uptime monitors) default to `/health`; aliasing avoids
        // a misleading 401 in their logs when they probe an unmatched
        // path. See auth_middleware's bypass list for the matching
        // allowance.
        .route("/health", get(health))
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
            get(crate::a2a::server::agent_card_handler).layer(axum::middleware::from_fn(
                crate::a2a::version::a2a_version_layer,
            )),
        )
        // OpenAI-compatible endpoints — allow any OpenAI API client to connect.
        .route("/v1/chat/completions", post(openai_chat_completions))
        .route("/v1/models", get(openai_list_models))
        // OpenAI Files API — file upload/management for doubao and other providers.
        .route("/v1/files", post(upload_file).get(list_files))
        .route(
            "/v1/files/{file_id}",
            get(get_file_meta).delete(delete_file),
        )
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
        // H1: per-IP rate limiter (default 100 req/60s, configurable via gateway.rateLimitRps)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ))
        // H2: restrict CORS when not on loopback
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Start the HTTP server. Blocks until shutdown.
///
/// On `state.shutdown.begin_drain()` (called by `POST /api/v1/restart`), axum
/// stops accepting new connections, lets in-flight requests complete, and then
/// returns. The caller (`serve`'s caller in `startup.rs`) is responsible for
/// the post-drain steps (waiting for non-HTTP inflight, spawning the
/// replacement process, exiting).
pub async fn serve(state: AppState, bind: std::net::SocketAddr) -> Result<()> {
    let shutdown = state.shutdown.clone();
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("gateway listening on {bind}");
    // `into_make_service_with_connect_info` exposes the peer SocketAddr to
    // handlers via `ConnectInfo<SocketAddr>` — needed by /shutdown /restart
    // to enforce loopback-only access.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move { shutdown.notified().await })
    .await?;
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
    // Health, agent card discovery, WS, and internal reload endpoints are always
    // open (WS performs its own handshake-level auth).
    //
    // Both the bare `/health` and the namespaced `/api/v1/health` are
    // listed: the bare path is the default for container orchestrators
    // (Docker HEALTHCHECK, k8s probes, generic uptime monitors) and
    // operator muscle memory; without the bypass the auth middleware
    // would respond 401 BEFORE the request reached the routing layer,
    // surfacing as a misleading "unauthorized" instead of the honest
    // "path does not exist on this server". Both routes resolve to the
    // same handler — see the top-level alias just below the `nest`
    // call in `build_router`.
    let path = request.uri().path();
    if path == "/"
        || path == "/health"
        || path == "/api/v1/health"
        || path == "/.well-known/agent.json"
        || path == "/ws"
        || path == "/gateway-ws"
        || path.starts_with("/hooks/")
        || path == "/api/v1/cron/reload"
        || path == "/api/v1/shutdown"
        || path == "/api/v1/restart"
        || path == "/api/v1/restart-dismiss"
        // A2A v1.0: bypass gateway-level auth so the protocol's own
        // bearer/X-API-Key schemes (declared in Agent Card securitySchemes
        // and enforced by `a2a_auth_layer`) are the authoritative gate.
        // Otherwise the gateway token would override the A2A-advertised
        // auth and any A2A v1.0 client following the spec would 401.
        || path == "/api/v1/a2a"
        || path == "/api/v1/a2a/relay/ws"
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

// H1: per-IP rate limiting middleware. Uses a simple sliding-window counter.
async fn rate_limit_middleware(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let ip = addr.ip();
    const DEFAULT_MAX_REQ: u32 = 100;
    const WINDOW_SECS: u64 = 60;
    if state.rate_limiter.check(ip, DEFAULT_MAX_REQ, WINDOW_SECS).await {
        warn!(%ip, DEFAULT_MAX_REQ, "rate limit exceeded");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "rate limit exceeded"})),
        )
            .into_response();
    }
    next.run(request).await
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
    info!(agent_id = ?req.agent_id, session_key = ?req.session_key, text_len = req.text.len(), stream = req.stream, "HTTP /api/v1/message");
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
    let (text, file_images, file_files) = rsclaw_agent::registry::extract_file_refs(&req.text);

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text,
        channel: req.channel.unwrap_or_else(|| "api".to_string()),
        peer_id: req.peer_id.unwrap_or_else(|| "api-client".to_string()),
        chat_id: String::new(),
        reply_tx,
        task_id: None,
        context_id: None,
        event_tx: None,
        cancel_token: None,
        input_request_tx: None,
        extra_tools: vec![],
        images: file_images,
        files: file_files,
        account: None,
    };

    // Subscribe to event_bus BEFORE sending message so we don't miss early deltas.
    let event_rx = if req.stream {
        Some(state.event_bus.subscribe())
    } else {
        None
    };

    if handle.tx.send(msg).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "agent inbox closed"})),
        )
            .into_response();
    }

    // Streaming: return SSE immediately in OpenAI-chunk format.
    if let Some(rx) = event_rx {
        let sid = session_key.clone();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cid = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
        // Track this streaming request in the gateway's inflight count.
        // Guard moves into the filter_map closure and drops with the stream.
        let inflight_guard = state.shutdown.begin_work();
        let shutdown_for_stream = state.shutdown.clone();

        let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
            .filter_map(move |msg| {
                let _hold_inflight = &inflight_guard;
                let sid = sid.clone();
                let cid = cid.clone();
                async move {
                    let event = msg.ok()?;
                    if event.session_id != sid { return None; }
                    if event.done {
                        let mut stop = serde_json::json!({
                            "id": cid, "object": "chat.completion.chunk",
                            "created": now, "model": "rsclaw",
                            "choices": [{"index":0,"delta":{},"finish_reason":"stop"}]
                        });
                        if !event.files.is_empty() {
                            stop["rsclaw_files"] = serde_json::json!(event.files);
                        }
                        if !event.images.is_empty() {
                            stop["rsclaw_images"] = serde_json::json!(event.images);
                        }
                        if !event.tool_log.is_empty() {
                            stop["rsclaw_tool_log"] = serde_json::json!(event.tool_log);
                        }
                        return Some(format!("data: {stop}\n\ndata: [DONE]\n\n"));
                    }
                    if event.delta.is_empty() { return None; }
                    let chunk = serde_json::json!({
                        "id": cid, "object": "chat.completion.chunk",
                        "created": now, "model": "rsclaw",
                        "choices": [{"index":0,"delta":{"content":event.delta},"finish_reason":null}]
                    });
                    Some(format!("data: {chunk}\n\n"))
                }
            })
            .scan(false, |done, line| {
                if *done { return std::future::ready(None); }
                if line.contains("[DONE]") { *done = true; }
                std::future::ready(Some(Ok::<_, Infallible>(line)))
            })
            .take_until(Box::pin(async move { shutdown_for_stream.notified().await }));

        let mut hdrs = axum::http::HeaderMap::new();
        hdrs.insert(
            header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8".parse().expect("ct"),
        );
        hdrs.insert(header::CACHE_CONTROL, "no-cache".parse().expect("cc"));
        hdrs.insert(
            "x-accel-buffering"
                .parse::<axum::http::HeaderName>()
                .expect("hdr"),
            "no".parse().expect("v"),
        );
        return (StatusCode::OK, hdrs, axum::body::Body::from_stream(stream)).into_response();
    }

    // Non-streaming: track inflight while we await the agent's full reply.
    let _inflight_guard = state.shutdown.begin_work();
    let shutdown_for_wait = state.shutdown.clone();
    let timeout_secs = state
        .config
        .raw
        .agents
        .as_ref()
        .and_then(|a| a.defaults.as_ref())
        .and_then(|d| d.timeout_seconds)
        .unwrap_or(600) as u64;
    let reply = match tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(timeout_secs), reply_rx) => r,
        () = shutdown_for_wait.notified() => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "gateway draining"})),
            ).into_response();
        }
    } {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn media_path_is_limited_to_allowed_roots() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        let var = root.path().join("var");
        let downloads = root.path().join("Downloads").join("rsclaw");
        let outside = root.path().join("outside");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace");
        tokio::fs::create_dir_all(&var).await.expect("var");
        tokio::fs::create_dir_all(&downloads)
            .await
            .expect("downloads");
        tokio::fs::create_dir_all(&outside).await.expect("outside");

        let workspace_file = workspace.join("chart.png");
        let var_file = var.join("chart.png");
        let downloads_file = downloads.join("chart.png");
        let outside_file = outside.join("chart.png");
        tokio::fs::write(&workspace_file, b"png")
            .await
            .expect("workspace file");
        tokio::fs::write(&var_file, b"png").await.expect("var file");
        tokio::fs::write(&downloads_file, b"png")
            .await
            .expect("downloads file");
        tokio::fs::write(&outside_file, b"png")
            .await
            .expect("outside file");

        let roots = vec![workspace.clone(), var, downloads];
        assert!(
            canonicalize_media_path_in_roots("chart.png", &workspace, roots.clone())
                .await
                .is_ok()
        );
        assert!(
            canonicalize_media_path_in_roots(
                var_file.to_string_lossy().as_ref(),
                &workspace,
                roots.clone()
            )
            .await
            .is_ok()
        );
        assert!(
            canonicalize_media_path_in_roots(
                downloads_file.to_string_lossy().as_ref(),
                &workspace,
                roots.clone()
            )
            .await
            .is_ok()
        );
        assert!(
            canonicalize_media_path_in_roots(
                outside_file.to_string_lossy().as_ref(),
                &workspace,
                roots
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn media_path_rejects_large_or_non_file_inputs() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace");
        let large = workspace.join("large.png");
        let file = std::fs::File::create(&large).expect("large file");
        file.set_len(MAX_LOCAL_MEDIA_BYTES + 1).expect("set len");
        drop(file);

        assert!(
            canonicalize_media_path_in_roots("large.png", &workspace, vec![workspace.clone()])
                .await
                .is_err()
        );
        assert!(
            canonicalize_media_path_in_roots(".", &workspace, vec![workspace.clone()])
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn media_path_rejects_symlink_escape() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace");
        tokio::fs::create_dir_all(&outside).await.expect("outside");
        let outside_file = outside.join("secret.png");
        let link = workspace.join("linked.png");
        tokio::fs::write(&outside_file, b"secret")
            .await
            .expect("outside file");
        std::os::unix::fs::symlink(&outside_file, &link).expect("symlink");

        assert!(
            canonicalize_media_path_in_roots("linked.png", &workspace, vec![workspace.clone()])
                .await
                .is_err()
        );
    }

    #[test]
    fn media_mime_only_allows_supported_images() {
        assert_eq!(
            media_mime_for_path(std::path::Path::new("x.png")).expect("png"),
            "image/png"
        );
        assert_eq!(
            media_mime_for_path(std::path::Path::new("x.jpeg")).expect("jpeg"),
            "image/jpeg"
        );
        assert!(media_mime_for_path(std::path::Path::new("id_rsa")).is_err());
    }
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
            model: h
                .config
                .model
                .as_ref()
                .and_then(|m| m.primary_head().map(String::from)),
            default: h.config.default == Some(true),
        })
        .collect();
    Json(agents)
}

/// List active ACP/WS connections with client metadata.
async fn list_acp_connections(State(state): State<AppState>) -> impl IntoResponse {
    let conns = state.ws_conns.list_connections().await;
    Json(conns)
}

// ---------------------------------------------------------------------------
// Message routing — send/read/broadcast via channel_senders
// ---------------------------------------------------------------------------

/// POST /api/v1/message/send — send a message to a channel target.
///
/// Body fields:
///   - `target`  (required): channel-native target id (feishu open_id, wechat
///     openid, etc.)
///   - `channel` (required): channel brand (feishu / wechat / …)
///   - `message` (required unless `media` is supplied): text body
///   - `media`   (optional): single attachment, can be:
///       * a local file path  (`/abs/path.png` or `~/...` expanded)
///       * an http/https URL  (downstream channel downloads it)
///       * a `data:image/...;base64,...` URI (passed through)
///     File-path PNG/JPEG/WEBP are slurped + base64-encoded into a data URI
///     here so the channel sender only deals with URI / URL forms.
///   - `replyTo` (optional): channel-native reply-anchor id
///   - `account` (optional): account name to send from; bare `{channel}` if
///     absent
async fn message_send(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let target = body["target"].as_str().unwrap_or("");
    let text = body["message"].as_str().unwrap_or("");
    let channel = body["channel"].as_str().unwrap_or("");
    let media = body["media"].as_str().unwrap_or("");
    let account = body["account"]
        .as_str()
        .map(str::to_owned)
        .filter(|s| !s.is_empty());

    if target.is_empty() || channel.is_empty() || (text.is_empty() && media.is_empty()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "missing required fields: target, channel, and (message or media)"
            })),
        );
    }

    let images = if media.is_empty() {
        vec![]
    } else {
        match resolve_media_to_image_data(media).await {
            Ok(uri) => vec![uri],
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": format!("media resolve failed: {e}")})),
                );
            }
        }
    };

    let out = rsclaw_channel::OutboundMessage {
        target_id: target.to_string(),
        is_group: false,
        text: text.to_string(),
        reply_to: body["replyTo"].as_str().map(str::to_owned),
        images,
        files: vec![],
        channel: Some(channel.to_string()),
        account,
    };

    match state.notification_tx.send(out) {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("send failed: {e}")})),
        ),
    }
}

/// Resolve a `media` body field into a channel-friendly image reference.
///
/// Returns the input unchanged for `data:` URIs and `http(s)://` URLs (the
/// feishu sender already handles both). File paths are read, base64-encoded
/// and reformatted as `data:image/<mime>;base64,<...>`; the channel sender
/// only knows about URI/URL forms, so this is where the path → URI bridge
/// lives.
async fn resolve_media_to_image_data(media: &str) -> anyhow::Result<String> {
    if media.starts_with("data:") || media.starts_with("http://") || media.starts_with("https://") {
        return Ok(media.to_string());
    }

    let expanded = canonicalize_allowed_media_path(media).await?;
    let mime = media_mime_for_path(&expanded)?;

    let bytes = tokio::fs::read(&expanded)
        .await
        .map_err(|e| anyhow::anyhow!("read {}: {e}", expanded.display()))?;

    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

async fn canonicalize_allowed_media_path(media: &str) -> anyhow::Result<PathBuf> {
    let base = rsclaw_config::loader::base_dir();
    let workspace = base.join("workspace");
    let roots = allowed_media_roots(&base);
    canonicalize_media_path_in_roots(media, &workspace, roots).await
}

async fn canonicalize_media_path_in_roots(
    media: &str,
    workspace: &std::path::Path,
    roots: Vec<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let lexical = rsclaw_util::canonicalize_external_path(media, &workspace);
    let meta = tokio::fs::metadata(&lexical)
        .await
        .map_err(|e| anyhow::anyhow!("stat {}: {e}", lexical.display()))?;
    if !meta.is_file() {
        anyhow::bail!("media path is not a regular file: {}", lexical.display());
    }
    if meta.len() > MAX_LOCAL_MEDIA_BYTES {
        anyhow::bail!(
            "media file too large: {} bytes exceeds {} bytes",
            meta.len(),
            MAX_LOCAL_MEDIA_BYTES
        );
    }
    let canonical = tokio::fs::canonicalize(&lexical)
        .await
        .map_err(|e| anyhow::anyhow!("canonicalize {}: {e}", lexical.display()))?;
    for root in roots {
        let root_canonical = tokio::fs::canonicalize(&root).await.unwrap_or(root);
        if canonical.starts_with(&root_canonical) {
            return Ok(canonical);
        }
    }
    anyhow::bail!(
        "media path '{}' resolves outside allowed dirs (workspace, var, or Downloads/rsclaw)",
        media
    )
}

fn allowed_media_roots(base: &std::path::Path) -> Vec<PathBuf> {
    let downloads_rsclaw = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(rsclaw_config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw");
    vec![base.join("workspace"), base.join("var"), downloads_rsclaw]
}

fn media_mime_for_path(path: &std::path::Path) -> anyhow::Result<&'static str> {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Ok("image/png"),
        Some("jpg") | Some("jpeg") => Ok("image/jpeg"),
        Some("webp") => Ok("image/webp"),
        _ => anyhow::bail!("unsupported local media extension: {}", path.display()),
    }
}

/// GET /api/v1/message/read — read recent messages from a session.
async fn message_read(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let target = params.get("target").map(String::as_str).unwrap_or("");
    let channel = params.get("channel").map(String::as_str).unwrap_or("");
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    if target.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing required field: target"})),
        );
    }

    // Find session: exact match first, then substring search.
    let sessions = state.store.db.list_sessions().unwrap_or_default();

    let session_key = if sessions.iter().any(|s| s == target) {
        // Target is a full session key.
        target.to_string()
    } else {
        // Substring search: channel:target or just target.
        let needle = if channel.is_empty() {
            target.to_string()
        } else {
            format!("{channel}:{target}")
        };
        match sessions.iter().filter(|s| s.contains(&needle)).next_back() {
            Some(s) => s.clone(),
            None => return (StatusCode::OK, Json(serde_json::json!([]))),
        }
    };

    let messages = state
        .store
        .db
        .load_messages(&session_key)
        .unwrap_or_default();
    let recent: Vec<_> = messages
        .into_iter()
        .rev()
        .take(limit)
        .map(rsclaw_provider::redact_rsclaw_hidden_value)
        .collect::<Vec<serde_json::Value>>()
        .into_iter()
        .rev()
        .collect();
    (StatusCode::OK, Json(serde_json::json!(recent)))
}

/// POST /api/v1/message/broadcast — send the same message to multiple targets.
async fn message_broadcast(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let channel = body["channel"].as_str().unwrap_or("");
    let text = body["message"].as_str().unwrap_or("");
    let targets = body["targets"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    let account = body["account"]
        .as_str()
        .map(str::to_owned)
        .filter(|s| !s.is_empty());

    if text.is_empty() || channel.is_empty() || targets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": "missing required fields: channel, message, targets"}),
            ),
        );
    }

    let mut sent = 0u32;
    let mut failed = 0u32;
    for target in &targets {
        let out = rsclaw_channel::OutboundMessage {
            target_id: target.to_string(),
            is_group: false,
            text: text.to_string(),
            reply_to: None,
            images: vec![],
            files: vec![],
            channel: Some(channel.to_string()),
            account: account.clone(),
        };
        match state.notification_tx.send(out) {
            Ok(_) => sent += 1,
            Err(_) => failed += 1,
        }
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({"sent": sent, "failed": failed})),
    )
}

async fn agent_status(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.agents.get(&id) {
        Ok(h) => Json(AgentStatusResponse {
            id: h.id.clone(),
            model: h
                .config
                .model
                .as_ref()
                .and_then(|m| m.primary_head().map(String::from)),
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
            if let Err(e) = rsclaw_agent::bootstrap::seed_workspace(&ws) {
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
        )
            .into_response();
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

    // Check WASM plugins first; wasm wins on collision (matches agent dispatch).
    if let Some((plugin_name, tool_inner)) = tool_name.split_once('.') {
        for wp in state.wasm_plugins.iter() {
            if wp.name == plugin_name {
                match wp.call_tool(tool_inner, args.clone()).await {
                    Ok(result) => return Json(serde_json::json!({"ok": true, "result": result})),
                    Err(e) => {
                        return Json(serde_json::json!({"ok": false, "error": format!("{e:#}")}));
                    }
                }
            }
        }
        // Fall through to JS plugins. The REST endpoint has no IM session
        // context, so _ctx fields are empty — host.notify will return
        // logged_only rather than dispatching.
        if let Some(plugin) = state.plugins.get_js(plugin_name) {
            let params = serde_json::json!({
                "tool": tool_inner,
                "args": args,
                "_ctx": { "target_id": "", "channel": "", "session_key": "" }
            });
            return match plugin.call("tool_call", params).await {
                Ok(result) => Json(serde_json::json!({"ok": true, "result": result})),
                Err(e) => Json(serde_json::json!({"ok": false, "error": format!("{e:#}")})),
            };
        }
        return Json(serde_json::json!({"error": format!("plugin '{}' not found", plugin_name)}));
    }

    Json(serde_json::json!({"error": "use 'plugin.tool' format, e.g. 'jimeng.txt2img'"}))
}

// ---------------------------------------------------------------------------
// Hub catalog (read-only) — for the desktop tools/skills/plugins module.
// Available (from the signed allowlist/tools manifest) + installed status.
// Lazy-fetches the caches if missing; fail-open (returns cached/empty on hub
// down).
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SkillCatalogEntry {
    slug: String,
    version: String,
    installed: bool,
    publisher: String,
    description: String,
}

#[derive(Serialize)]
struct PluginCatalogEntry {
    slug: String,
    version: String,
    installed: bool,
    description: String,
}

#[derive(Serialize)]
struct HubCatalogResponse {
    tools: Vec<crate::cmd::tools::ToolCatalogEntry>,
    skills: Vec<SkillCatalogEntry>,
    plugins: Vec<PluginCatalogEntry>,
}

fn installed_skill(slug: &str) -> bool {
    rsclaw_config::loader::base_dir()
        .join("skills")
        .join(slug)
        .join("SKILL.md")
        .is_file()
}

fn installed_plugin(slug: &str) -> bool {
    rsclaw_config::loader::base_dir()
        .join("plugins")
        .join(slug)
        .is_dir()
}

async fn build_skill_catalog() -> Vec<SkillCatalogEntry> {
    if rsclaw_skill::allowlist::snapshot().counts() == (0, 0) {
        let _ = rsclaw_skill::allowlist::refresh().await; // lazy; fail-open
    }
    rsclaw_skill::allowlist::snapshot()
        .skills_sorted()
        .into_iter()
        .map(|e| SkillCatalogEntry {
            installed: installed_skill(&e.slug),
            slug: e.slug,
            version: e.version,
            publisher: e.publisher,
            description: e.description,
        })
        .collect()
}

async fn build_plugin_catalog() -> Vec<PluginCatalogEntry> {
    if rsclaw_skill::allowlist::snapshot().counts() == (0, 0) {
        let _ = rsclaw_skill::allowlist::refresh().await;
    }
    rsclaw_skill::allowlist::snapshot()
        .plugins_sorted()
        .into_iter()
        .map(|e| PluginCatalogEntry {
            installed: installed_plugin(&e.slug),
            slug: e.slug,
            version: e.version,
            description: e.description,
        })
        .collect()
}

/// GET /api/v1/hub/catalog — tools + skills + plugins in one shot.
async fn hub_catalog() -> impl IntoResponse {
    crate::cmd::tools::ensure_manifest_cached().await;
    Json(HubCatalogResponse {
        tools: crate::cmd::tools::tools_catalog(),
        skills: build_skill_catalog().await,
        plugins: build_plugin_catalog().await,
    })
}

/// GET /api/v1/hub/skills
async fn hub_skills() -> impl IntoResponse {
    Json(build_skill_catalog().await)
}

/// GET /api/v1/hub/plugins
async fn hub_plugins() -> impl IntoResponse {
    Json(build_plugin_catalog().await)
}

/// GET /api/v1/plugins/{name}/tools — describe one plugin's tool surface.
///
/// Returns `{ plugin, runtime: "wasm"|"js", tools: [{ name, description,
/// parameters }] }`. WASM plugins win on slug collision (matches
/// /tools/execute dispatch order). The shape is intentionally agent-friendly
/// — `rsclaw plugin describe <name>` and `rsclaw plugin call` consume this
/// to know what args a given tool wants without the agent having to read
/// plugin manifests off disk.
async fn plugin_describe(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    let name = name.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "plugin name required"})),
        )
            .into_response();
    }
    if let Some(wp) = state.wasm_plugins.iter().find(|p| p.name == name) {
        let tools: Vec<serde_json::Value> = wp
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "headline": t.headline,
                })
            })
            .collect();
        return Json(serde_json::json!({
            "plugin": wp.name,
            "runtime": "wasm",
            "tools": tools,
        }))
        .into_response();
    }
    if let Some(plugin) = state.plugins.get_js(name) {
        let tools: Vec<serde_json::Value> = plugin
            .manifest
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                })
            })
            .collect();
        return Json(serde_json::json!({
            "plugin": plugin.manifest.name,
            "runtime": "js",
            "tools": tools,
        }))
        .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": format!("plugin '{name}' not found")})),
    )
        .into_response()
}

/// GET /api/v1/hub/tools
async fn hub_tools() -> impl IntoResponse {
    crate::cmd::tools::ensure_manifest_cached().await;
    Json(crate::cmd::tools::tools_catalog())
}

async fn health(State(_state): State<AppState>) -> impl IntoResponse {
    // Minimal unauthenticated health payload (R4 review I2).
    // Previously leaked `version` + `port` to anyone reaching the
    // bare `/health` alias — useful for CVE fingerprinting against
    // a public-facing gateway, and the port is redundant since the
    // caller already connected to it. Rich uptime / build info now
    // lives only on the auth-gated `/api/v1/status` endpoint.
    Json(serde_json::json!({"status": "ok"}))
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
        check_ch!(
            telegram, discord, slack, whatsapp, signal, feishu, dingtalk, wecom, wechat, qq, line,
            zalo, matrix
        );
        chs
    };

    // Active session count: sessions with activity in the last 24h.
    let sessions = {
        let all = state.store.db.list_sessions().unwrap_or_default();
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - 86400;
        all.iter()
            .filter(|key| {
                state
                    .store
                    .db
                    .get_session_meta(key)
                    .ok()
                    .flatten()
                    .map(|m| m.last_active > cutoff)
                    .unwrap_or(false)
            })
            .count()
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
                    if kb > 1024 {
                        format!("{} MB", kb / 1024)
                    } else {
                        format!("{} KB", kb)
                    }
                })
                .unwrap_or_else(|| "--".into())
        }
        #[cfg(not(target_os = "macos"))]
        {
            "--".to_string()
        }
    };

    // Disabled providers — Phase 3 surface. Empty list when every
    // configured provider booted cleanly; non-empty when one or more
    // had unresolved Secret fields. Operators see the reason here
    // without grep'ing logs. All agents share the same ProviderRegistry
    // Arc, so any agent (the first one) is sufficient.
    let (providers_active, providers_disabled): (Vec<String>, Vec<serde_json::Value>) =
        match state.agents.all().first() {
            Some(agent) => {
                let regs = &agent.providers;
                let active: Vec<String> = regs.names().into_iter().map(str::to_owned).collect();
                let disabled: Vec<serde_json::Value> = regs
                    .disabled_list()
                    .into_iter()
                    .map(|(name, reason)| serde_json::json!({"name": name, "reason": reason}))
                    .collect();
                (active, disabled)
            }
            None => (Vec::new(), Vec::new()),
        };

    Json(serde_json::json!({
        "version": option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"),
        "agents": state.agents.len(),
        "port": port,
        "uptime": uptime,
        "memory": memory,
        "sessions": sessions,
        "channels": channels,
        "providers": {
            "active": providers_active,
            "disabled": providers_disabled,
        },
    }))
}

async fn config_reload(State(_state): State<AppState>) -> impl IntoResponse {
    // Re-load config from disk and broadcast a reload event.
    // Full hot-reload wiring is completed in the gateway startup path;
    // here we just validate the config is still parseable.
    match rsclaw_config::load() {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"reloaded": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Reject the request if the caller isn't a loopback peer.
/// Shutdown / restart are only exposed to local processes — never over LAN.
///
/// Caveat: this checks the direct **TCP peer** address, NOT `X-Forwarded-For`.
/// If you run rsclaw behind a reverse proxy (nginx/caddy/traefik), the proxy
/// itself may be loopback even when the original client is on the WAN, which
/// would incorrectly authorize remote shutdowns. In that deployment, bind the
/// gateway to 127.0.0.1 so only the proxy can reach it, OR put `/shutdown` and
/// `/restart` behind an explicit proxy ACL.
pub(crate) fn is_loopback(addr: std::net::SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// POST /api/v1/shutdown — exit the gateway process cleanly.
/// Loopback-only; no token required (same trust model as /api/v1/cron/reload).
async fn http_shutdown(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Response {
    if !is_loopback(peer) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "shutdown is loopback-only"})),
        )
            .into_response();
    }
    tracing::warn!("HTTP /shutdown — exiting in 300ms");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = std::fs::remove_file(rsclaw_config::loader::pid_file());
        std::process::exit(0);
    });
    Json(serde_json::json!({ "shutting_down": true })).into_response()
}

/// POST /api/v1/restart — re-exec the gateway binary with graceful drain.
/// Loopback-only.
///
/// Flag-and-flush design:
///   1. Set `shutdown.restart_requested = true` and `draining = true` — axum's
///      `with_graceful_shutdown` future resolves, the listener accept loop
///      stops, in-flight HTTP connections (including this response) finish.
///   2. Send the "restarting" response so the client knows the request landed.
///   3. `axum::serve` returns; `start_gateway` notices `is_restart_requested`
///      and spawns the replacement process AFTER the listener has been released
///      — closing the race where the child's `bind()` would fail because the
///      parent still owns the port.
///
/// Persistent task queue items left behind are picked up by the new process
/// on startup, so the user-visible behavior is "brief unavailability".
async fn http_restart(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Response {
    if !is_loopback(peer) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "restart is loopback-only"})),
        )
            .into_response();
    }
    tracing::warn!("HTTP /restart — flagging for post-drain respawn");
    state.shutdown.request_restart();
    Json(serde_json::json!({ "restarting": true })).into_response()
}

/// POST /api/v1/restart-dismiss — clear the pending-restart latch so the UI
/// banner stops re-appearing on reconnect. New restart-required events
/// re-populate the latch and re-show the banner. Loopback-only.
async fn http_restart_dismiss(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Response {
    if !is_loopback(peer) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "restart-dismiss is loopback-only"})),
        )
            .into_response();
    }
    if let Ok(mut guard) = state.pending_restart.write() {
        *guard = None;
    }
    Json(serde_json::json!({ "dismissed": true })).into_response()
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

/// PUT /api/v1/cron/bulk_replace — atomically replace the entire cron
/// job set with the supplied list. Used by the Tauri tray (see
/// `ui/src-tauri/src/main.rs::save_cron_jobs`) so the UI never needs
/// to write the cron.json5 file directly — preventing the 3-writer
/// race where Tauri / ws cron.update / cron runner all hit the same
/// file. Body shape: `{ "version": 1, "jobs": [...] }` (matches
/// `cron.json5`).
async fn cron_bulk_replace(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let jobs_array = match body.get("jobs").and_then(|v| v.as_array()) {
        Some(arr) => arr.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing `jobs` array"})),
            )
                .into_response();
        }
    };
    let jobs: Vec<crate::cron::CronJob> = match jobs_array
        .iter()
        .map(|v| serde_json::from_value::<crate::cron::CronJob>(v.clone()))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(j) => j,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid cron job: {e}")})),
            )
                .into_response();
        }
    };

    // Serialize against concurrent cron.add / cron.update etc.
    let _guard = crate::cron::CRON_FILE_LOCK.lock().await;

    if let Err(e) = crate::cron::save_cron_jobs(&jobs) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("save failed: {e}")})),
        )
            .into_response();
    }

    // Notify CronRunner to reload (drops in-flight tasks for jobs
    // whose schedule / enabled state changed).
    if let Err(e) = state.cron_reload.send(()) {
        tracing::warn!(err = %e, "cron: bulk_replace reload signal failed");
    }

    Json(serde_json::json!({ "replaced": jobs.len() })).into_response()
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
    // Write the openclaw-compatible `{version, jobs}` envelope — same format
    // that cron::save_cron_jobs (CLI / WS) and the Tauri save_cron_jobs
    // command produce. Writing a bare array here made the file unloadable
    // for other readers and caused jobs to silently disappear from the UI.
    let store = serde_json::json!({ "version": 1, "jobs": jobs });
    let json = serde_json::to_string_pretty(&store).map_err(|e| format!("serialize: {e}"))?;
    let tmp = format!("{}.tmp", path.display());
    tokio::fs::write(&tmp, json)
        .await
        .map_err(|e| format!("write jobs.json tmp: {e}"))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| format!("rename jobs.json: {e}"))?;
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
    if let Some(sched) = body
        .get("schedule")
        .and_then(|s| s.as_str())
        .map(|s| s.to_owned())
    {
        // Validate before normalizing — reject bad expressions with a friendly error.
        if let Err(msg) = crate::cron::validate_cron_expr(&sched) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response();
        }
        let tz = body
            .get("timezone")
            .and_then(|t| t.as_str())
            .map(|t| t.to_owned());
        if let Some(tz) = tz {
            body["schedule"] = serde_json::json!({"kind": "cron", "expr": sched, "tz": tz});
        } else {
            body["schedule"] = serde_json::json!(sched);
        }
        // Remove timezone since it's now in the schedule object
        if let Some(obj) = body.as_object_mut() {
            obj.remove("timezone");
        }
    } else if let Some(expr) = body
        .get("schedule")
        .and_then(|s| s.get("expr"))
        .and_then(|e| e.as_str())
    {
        // Nested schedule form — validate the expr here too.
        if let Err(msg) = crate::cron::validate_cron_expr(expr) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response();
        }
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    body["created_at_ms"] = serde_json::json!(now_ms);
    body["updated_at_ms"] = serde_json::json!(now_ms);

    // Serialize read-modify-write so parallel cron.add calls don't clobber each
    // other.
    let _guard = crate::cron::CRON_FILE_LOCK.lock().await;
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
    let _guard = crate::cron::CRON_FILE_LOCK.lock().await;
    let mut jobs = cron_load_jobs().await;
    let idx = match jobs.iter().position(|j| j["id"].as_str() == Some(&id)) {
        Some(i) => i,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "job not found"})),
            )
                .into_response();
        }
    };

    // Merge fields from body into existing job
    if let Some(existing) = jobs[idx].as_object_mut() {
        if let Some(patch) = body.as_object() {
            for (k, v) in patch {
                // Normalize schedule string + timezone
                if k == "schedule" {
                    if let Some(sched) = v.as_str() {
                        if let Err(msg) = crate::cron::validate_cron_expr(sched) {
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({"error": msg})),
                            )
                                .into_response();
                        }
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
    let _guard = crate::cron::CRON_FILE_LOCK.lock().await;
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
                .into_response();
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
    // After the agent replies, deliver the result through the job's delivery
    // channel.
    if let Ok(handle) = state.agents.get(agent_id) {
        let session_key = format!("cron:{}", id);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = rsclaw_agent::AgentMessage {
            session_key,
            text: message,
            channel: "cron".to_string(),
            peer_id: format!("cron:{id}"),
            chat_id: String::new(),
            reply_tx,
            task_id: None,
            context_id: None,
            event_tx: None,
            cancel_token: None,
            input_request_tx: None,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
            account: None,
        };
        if handle.tx.send(msg).await.is_ok() {
            // Deliver agent reply through the job's delivery config. Resolve
            // recipients with the SAME logic as the scheduled path (send_delivery)
            // so manual triggers honor `to` lists, the `agent.channels` sentinel,
            // and the no-`to` "reply to recent conversation" fallback.
            let delivery: Option<rsclaw_config::schema::CronDelivery> =
                serde_json::from_value(job["delivery"].clone()).ok();
            let agents = state.agents.clone();
            let agent_id_owned = agent_id.to_owned();
            let ntx = state.notification_tx.clone();
            let job_id = id.clone();
            let ws_conns = state.ws_conns.clone();
            let job_name = job["name"].as_str().unwrap_or(&id).to_owned();
            tokio::spawn(async move {
                if let Ok(reply) = reply_rx.await {
                    // Build notification text: use reply if non-empty, else fallback summary
                    let notify_text = if !reply.text.is_empty() {
                        reply.text.clone()
                    } else {
                        format!("定时任务执行完成: {}", job_name)
                    };
                    // Push result to desktop via WebSocket notification
                    let frame = EventFrame::new(
                        "notification",
                        serde_json::json!({ "text": notify_text }),
                        0,
                    );
                    ws_conns.broadcast_all(frame).await;

                    if !reply.text.is_empty() {
                        if let Some(delivery) = delivery {
                            if delivery.mode.as_deref() != Some("none") {
                                let thread = delivery.thread_id.clone();
                                let targets = crate::cron::resolve_delivery_targets(
                                    &agents,
                                    &agent_id_owned,
                                    &delivery,
                                );
                                for (channel_name, account, to, is_group) in targets {
                                    let resolved_channel = if channel_name == "ws" {
                                        "desktop".to_string()
                                    } else {
                                        channel_name
                                    };
                                    let _ = ntx.send(rsclaw_channel::OutboundMessage {
                                        target_id: to,
                                        is_group,
                                        text: reply.text.clone(),
                                        reply_to: thread.clone(),
                                        images: reply.images.clone(),
                                        files: reply.files.clone(),
                                        channel: Some(resolved_channel),
                                        account,
                                    });
                                }
                                tracing::info!(job_id = %job_id, "cron trigger: delivered reply to resolved recipients");
                            }
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

// ---------------------------------------------------------------------------
// Computer-use control endpoints (settings panel: bypass toggle +
// saved permissions list / revoke).
// ---------------------------------------------------------------------------

/// GET /api/v1/computer-use/permissions — list every persisted
/// "Always allow" grant for the settings UI.
async fn computer_use_permissions_list(State(state): State<AppState>) -> Response {
    match state.computer_permission.list_grants().await {
        Ok(grants) => (StatusCode::OK, Json(serde_json::json!({"grants": grants}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// DELETE /api/v1/computer-use/permissions/:agent_id/:app — revoke one
/// persisted grant. Idempotent: deleting a non-existent grant is
/// reported as 200 with `revoked: false`.
async fn computer_use_permissions_revoke(
    State(state): State<AppState>,
    Path((agent_id, app)): Path<(String, String)>,
) -> Response {
    use rsclaw_computer::permission::PermissionStore as _;
    match state.computer_permission.revoke(&agent_id, &app).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"revoked": true, "agent_id": agent_id, "app": app})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/v1/computer-use/bypass — read the current runtime value of
/// the bypass switch.
async fn computer_use_bypass_get(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "enabled": state.computer_permission.is_bypass_all(),
    }))
}

#[derive(Debug, Deserialize)]
struct BypassToggleBody {
    enabled: bool,
}

/// PUT /api/v1/computer-use/bypass — flip the bypass switch at runtime.
/// Does NOT write to the config file; restart reloads the config-defined
/// default.
async fn computer_use_bypass_set(
    State(state): State<AppState>,
    Json(body): Json<BypassToggleBody>,
) -> impl IntoResponse {
    state.computer_permission.set_bypass_all(body.enabled);
    Json(serde_json::json!({"enabled": body.enabled}))
}

/// POST /api/v1/computer-use/runs/:run_id/abort — flip the in-flight
/// run's abort flag. The driver loop checks the flag between steps and
/// exits with `DriverOutcome::UserAbort` -> emits a `Finished
/// { outcome_kind: "user_abort" }` status event, which the desktop
/// overlay consumes to fade itself out and restore the window.
///
/// Returns `{ aborted: true }` on hit, `{ aborted: false }` when the
/// run_id is unknown (already finished, never started, or wrong id).
/// Idempotent: aborting an already-aborted run still returns true on
/// the first call and false on subsequent calls (registry entry is
/// removed by the driver on exit).
async fn computer_use_run_abort(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> impl IntoResponse {
    let runs = state.computer_runs.read().await;
    let aborted = match runs.get(&run_id) {
        Some(flag) => {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        }
        None => false,
    };
    drop(runs);
    if aborted {
        tracing::info!(run_id = %run_id, "computer_use: abort requested");
    } else {
        tracing::warn!(run_id = %run_id, "computer_use: abort target not found (already finished?)");
    }
    Json(serde_json::json!({"aborted": aborted, "runId": run_id}))
}

/// GET /api/v1/computer-use/stream?session_id=...
///
/// HTTP/SSE shim for the existing WS-only computer_use relays. It forwards
/// `permission_request` and `computer_use_status` events from the same
/// broadcast channels used by the desktop WebSocket gateway.
async fn computer_use_stream_sse(
    State(state): State<AppState>,
    Query(_q): Query<std::collections::HashMap<String, String>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut permission_rx = state.computer_permission_tx.subscribe();
    let mut status_rx = state.computer_status_tx.subscribe();
    let stream = async_stream::stream! {
        loop {
            tokio::select! {
                result = permission_rx.recv() => {
                    match result {
                        Ok(req) => {
                            let data = serde_json::to_string(&serde_json::json!({
                                "type": "permission_request",
                                "request_id": req.request_id,
                                "agent_id": req.agent_id,
                                "app": req.app,
                                "reason": req.reason,
                                "estimated_steps": req.estimated_steps,
                            })).unwrap_or_else(|_| "{}".to_owned());
                            yield Ok(Event::default().data(data));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                result = status_rx.recv() => {
                    match result {
                        Ok(status) => {
                            let payload = serde_json::to_value(&status).unwrap_or(serde_json::json!({}));
                            let data = serde_json::to_string(&serde_json::json!({
                                "type": "computer_use_status",
                                "status": payload,
                            })).unwrap_or_else(|_| "{}".to_owned());
                            yield Ok(Event::default().data(data));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize)]
struct ComputerUsePermissionResponse {
    request_id: String,
    decision: String,
    agent_id: Option<String>,
    app: Option<String>,
}

async fn computer_use_permission_response(
    State(state): State<AppState>,
    Json(body): Json<ComputerUsePermissionResponse>,
) -> Response {
    use rsclaw_computer::permission::{PermissionDecision, PermissionStore as _};
    let decision = match body.decision.as_str() {
        "allow" | "allow_once" => PermissionDecision::AllowOnce,
        "allow_session" => PermissionDecision::AllowSession,
        "allow_always" => PermissionDecision::AllowAlways,
        "deny" => PermissionDecision::Deny,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!(
                        "invalid decision `{other}` (expected: allow_once, allow_session, allow_always, deny)"
                    )
                })),
            )
                .into_response();
        }
    };

    if let (Some(agent_id), Some(app)) = (body.agent_id.as_deref(), body.app.as_deref()) {
        if let Err(e) = state.computer_permission.record(agent_id, app, decision).await {
            warn!(error = %e, "computer_permission.record failed");
        }
    }

    let resolved = state
        .computer_permission
        .resolve_pending_request(&body.request_id, decision)
        .await;
    Json(serde_json::json!({
        "resolved": resolved,
        "request_id": body.request_id,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct GitQuery {
    path: Option<String>,
    staged: Option<bool>,
    limit: Option<usize>,
    repo: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitCommitRequest {
    path: Option<String>,
    message: String,
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GitReviewRequest {
    body: String,
    event: Option<String>,
}

fn git_workdir(path: Option<&str>) -> PathBuf {
    match path.filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

fn run_cmd(dir: &std::path::Path, program: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(dir);
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

async fn git_status(Query(q): Query<GitQuery>) -> Response {
    let dir = git_workdir(q.path.as_deref());
    let branch = run_cmd(&dir, "git", &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_owned())
        .trim()
        .to_owned();
    let porcelain = match run_cmd(&dir, "git", &["status", "--porcelain=v1", "--branch"]) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": error})),
            )
                .into_response();
        }
    };
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut ahead = 0;
    let mut behind = 0;
    for line in porcelain.lines() {
        if let Some(meta) = line.strip_prefix("## ") {
            if let Some(idx) = meta.find("ahead ") {
                ahead = meta[idx + 6..]
                    .split([',', ']'])
                    .next()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(0);
            }
            if let Some(idx) = meta.find("behind ") {
                behind = meta[idx + 7..]
                    .split([',', ']'])
                    .next()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(0);
            }
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let file = line[3..].to_owned();
        if !xy.starts_with(' ') && !xy.starts_with('?') {
            staged.push(file.clone());
        }
        if !xy.ends_with(' ') || xy.starts_with('?') {
            unstaged.push(file);
        }
    }
    Json(serde_json::json!({
        "branch": branch,
        "dirty": !staged.is_empty() || !unstaged.is_empty(),
        "staged": staged,
        "unstaged": unstaged,
        "ahead": ahead,
        "behind": behind,
    }))
    .into_response()
}

async fn git_diff(Query(q): Query<GitQuery>) -> Response {
    let dir = git_workdir(q.path.as_deref());
    let args = if q.staged.unwrap_or(false) {
        vec!["diff", "--staged"]
    } else {
        vec!["diff"]
    };
    match run_cmd(&dir, "git", &args) {
        Ok(diff) => (StatusCode::OK, diff).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

async fn git_log(Query(q): Query<GitQuery>) -> Response {
    let dir = git_workdir(q.path.as_deref());
    let limit = q.limit.unwrap_or(20).min(100).to_string();
    match run_cmd(&dir, "git", &["log", "--oneline", "-n", &limit]) {
        Ok(log) => {
            let commits: Vec<_> = log
                .lines()
                .map(|line| {
                    let mut parts = line.splitn(2, ' ');
                    serde_json::json!({
                        "sha": parts.next().unwrap_or(""),
                        "title": parts.next().unwrap_or(""),
                    })
                })
                .collect();
            Json(serde_json::json!({"commits": commits})).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

async fn git_commit(Json(body): Json<GitCommitRequest>) -> Response {
    let dir = git_workdir(body.path.as_deref());
    for file in &body.files {
        if let Err(error) = run_cmd(&dir, "git", &["add", "--", file]) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": error})),
            )
                .into_response();
        }
    }
    match run_cmd(&dir, "git", &["commit", "-m", &body.message]) {
        Ok(output) => Json(serde_json::json!({"ok": true, "output": output})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

async fn git_prs(Query(q): Query<GitQuery>) -> Response {
    let dir = git_workdir(q.repo.as_deref().or(q.path.as_deref()));
    match run_cmd(&dir, "gh", &["pr", "list", "--json", "number,title,state,author,url"]) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => Json(value).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        },
        Err(_) => Json(serde_json::json!([])).into_response(),
    }
}

async fn git_pr_get(Path(number): Path<u64>, Query(q): Query<GitQuery>) -> Response {
    let dir = git_workdir(q.repo.as_deref().or(q.path.as_deref()));
    let number_s = number.to_string();
    match run_cmd(
        &dir,
        "gh",
        &["pr", "view", &number_s, "--json", "number,title,state,author,url,body"],
    ) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => Json(value).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        },
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

async fn git_pr_review(Path(number): Path<u64>, Json(body): Json<GitReviewRequest>) -> Response {
    let number_s = number.to_string();
    let event = body.event.unwrap_or_else(|| "COMMENT".to_owned());
    let review_flag = match event.as_str() {
        "APPROVE" | "approve" => "--approve",
        "REQUEST_CHANGES" | "request_changes" => "--request-changes",
        _ => "--comment",
    };
    match run_cmd(
        &git_workdir(None),
        "gh",
        &["pr", "review", &number_s, review_flag, "--body", &body.body],
    ) {
        Ok(output) => Json(serde_json::json!({"ok": true, "output": output})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------

/// GET /api/v1/cron/:id/history — get run history for a cron job.
async fn cron_history(Path(id): Path<String>) -> impl IntoResponse {
    // Read run log from data dir
    let log_dir = rsclaw_config::loader::base_dir()
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
    let enforcers: Vec<(String, Arc<rsclaw_channel::DmPolicyEnforcer>)> =
        match state.dm_enforcers.read() {
            Ok(guard) => guard
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect(),
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
    let enforcers: Vec<(String, Arc<rsclaw_channel::DmPolicyEnforcer>)> =
        match state.dm_enforcers.read() {
            Ok(guard) => guard
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect(),
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
    let enforcers: Vec<(String, Arc<rsclaw_channel::DmPolicyEnforcer>)> =
        match state.dm_enforcers.read() {
            Ok(guard) => guard
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect(),
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
    let config_path = rsclaw_config::loader::detect_config_path()
        .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("rsclaw.json5"));
    match std::fs::read_to_string(&config_path) {
        Ok(content) => Json(serde_json::json!({
            "raw": content,
            "path": config_path.display().to_string(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
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
    let config_path = rsclaw_config::loader::detect_config_path()
        .unwrap_or_else(|| rsclaw_config::loader::base_dir().join("rsclaw.json5"));

    // Validate the new config parses before saving.
    if let Err(e) = json5::from_str::<serde_json::Value>(&req.raw) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("invalid config: {e}")})),
        )
            .into_response();
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
        )
            .into_response();
    }

    Json(serde_json::json!({
        "saved": true,
        "path": config_path.display().to_string(),
    }))
    .into_response()
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
                .map(rsclaw_provider::redact_rsclaw_hidden_value)
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

// is_compaction_message moved to
// rsclaw_agent::compaction::is_compaction_message
use rsclaw_agent::compaction::is_compaction_message;

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
fn parse_oai_tools(tools: Option<&serde_json::Value>) -> Vec<rsclaw_provider::ToolDef> {
    let Some(arr) = tools.and_then(|v| v.as_array()) else {
        return vec![];
    };
    arr.iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(rsclaw_provider::ToolDef {
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
    info!(stream = req.stream, model = ?req.model, "HTTP /v1/chat/completions");

    // C1: when gateway runs without auth (open mode), reject x- headers
    // from non-loopback sources to prevent header spoofing.
    let trusted_headers = {
        let gw = state.live.gateway.read().await;
        if gw.auth_token.is_none() {
            // Only trust headers when the request comes from localhost
            use std::net::{IpAddr, Ipv4Addr};
            let is_local = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<IpAddr>().ok())
                .map(|ip| ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(std::net::Ipv6Addr::LOCALHOST))
                .unwrap_or(false);
            if !is_local {
                info!("open gateway: rejecting spoofed x- headers from non-loopback");
                false
            } else {
                true
            }
        } else {
            // Auth middleware validated the token — headers are trusted
            true
        }
    };

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
    let session_key = if trusted_headers {
        headers
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
            })
    } else {
        // Untrusted: always use hash-based session key
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
    };

    let peer_id = if trusted_headers {
        headers
            .get("x-user-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("desktop")
            .to_owned()
    } else {
        "openapi".to_owned()
    };

    // Extract [file:path] references from user text.
    let (text, file_images, file_files) = rsclaw_agent::registry::extract_file_refs(&text);

    let extra_tools = parse_oai_tools(req.tools.as_ref());
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text,
        channel: if trusted_headers {
            headers
                .get("x-channel")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("desktop")
                .to_owned()
        } else {
            "openapi".to_owned()
        },
        peer_id,
        chat_id: String::new(),
        reply_tx,
        task_id: None,
        context_id: None,
        event_tx: None,
        cancel_token: None,
        input_request_tx: None,
        extra_tools,
        images: file_images,
        files: file_files,
        account: None,
    };

    // Subscribe to event_bus BEFORE sending message to agent,
    // so we don't miss early deltas for streaming responses.
    let event_rx = if req.stream {
        Some(state.event_bus.subscribe())
    } else {
        None
    };

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
        // Track this streaming request in the gateway's inflight count so a
        // graceful restart waits for the SSE stream to finish. The guard is
        // moved into the filter_map closure below; it drops when the stream
        // is dropped (client disconnect, [DONE] sent, or scan terminator).
        let inflight_guard = state.shutdown.begin_work();
        let shutdown_for_stream = state.shutdown.clone();

        let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
            .filter_map(move |msg| {
                let _hold_inflight = &inflight_guard;
                let sid = sid.clone();
                let cid = cid.clone();
                let model_str = model_str.clone();
                async move {
                    let event = msg.ok()?;
                    if event.session_id != sid { return None; }
                    if event.done {
                        let mut stop = serde_json::json!({
                            "id": cid, "object": "chat.completion.chunk",
                            "created": now, "model": model_str,
                            "choices": [{"index":0,"delta":{},"finish_reason":"stop"}]
                        });
                        if !event.files.is_empty() {
                            stop["rsclaw_files"] = serde_json::json!(event.files);
                        }
                        if !event.images.is_empty() {
                            stop["rsclaw_images"] = serde_json::json!(event.images);
                        }
                        if !event.tool_log.is_empty() {
                            stop["rsclaw_tool_log"] = serde_json::json!(event.tool_log);
                        }
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
            })
            .take_until(Box::pin(async move { shutdown_for_stream.notified().await }));

        let mut response_headers = axum::http::HeaderMap::new();
        response_headers.insert(
            header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8"
                .parse()
                .expect("header value"),
        );
        response_headers.insert(
            header::CACHE_CONTROL,
            "no-cache".parse().expect("header value"),
        );
        response_headers.insert(
            "x-accel-buffering"
                .parse::<axum::http::HeaderName>()
                .expect("header name"),
            "no".parse().expect("header value"),
        );

        return (
            StatusCode::OK,
            response_headers,
            axum::body::Body::from_stream(stream),
        )
            .into_response();
    }

    // Non-streaming: track inflight while we await the agent's full reply.
    let _inflight_guard = state.shutdown.begin_work();
    let shutdown_for_wait = state.shutdown.clone();
    let timeout_secs = state.config.agents.defaults.timeout_seconds.unwrap_or(600) as u64;
    let reply = match tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(timeout_secs), reply_rx) => r,
        () = shutdown_for_wait.notified() => {
            return (StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error":{"message":"gateway draining","type":"server_error"}}))).into_response();
        }
    } {
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
                .and_then(|m| m.primary_head())
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
    // We accept any verify_token for now (operator should secure via
    // WHATSAPP_VERIFY_TOKEN env).
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

    match serde_json::from_str::<rsclaw_channel::whatsapp::WebhookPayload>(&body) {
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
    use rsclaw_provider::defaults as prov_defaults;

    // Expand `${VAR}` placeholders so a config like `apiKey:
    // "${ANTHROPIC_API_KEY}"` tests the actual key, matching the gateway's
    // runtime expansion done in `config::loader::load_config`. Without this the
    // test sends the literal template string and Anthropic/etc. return 401.
    let raw_key = req.api_key.clone();
    let api_key = rsclaw_config::loader::expand_env_vars(&req.api_key);
    let base_url_in: Option<String> = req
        .base_url
        .as_deref()
        .map(rsclaw_config::loader::expand_env_vars);
    // Diagnose silent failure: user has `apiKey: "${FOO}"` but FOO is
    // unset/empty in the gateway's process env, so expansion → "" and
    // we'd send an empty key header. Returning early with a specific
    // error saves the user from chasing a misleading "invalid key".
    let needs_key = !matches!(req.provider.as_str(), "ollama");
    if needs_key && api_key.is_empty() && raw_key.contains("${") {
        let var = raw_key
            .trim()
            .trim_start_matches("${")
            .trim_end_matches('}');
        return Err(format!(
            "API key expanded to empty — env var '{var}' is unset or empty in the gateway process. \
             Either export it before starting the gateway, or replace the placeholder in rsclaw.json5 \
             with the literal key value."
        ));
    }

    // For custom/codingplan providers, resolve auth/URL based on api_type.
    // Doubao also opts in (CodingPlan offers an Anthropic-compat endpoint)
    // — but its base URL still defaults to ark so we only let api_type
    // override auth style, not the URL.
    let is_custom_like = req.provider == "custom" || req.provider == "codingplan";
    let supports_api_type = is_custom_like || req.provider == "doubao";
    let effective_type = if supports_api_type && req.api_type.is_some() {
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
    let base_url = if let Some(ref explicit) = base_url_in {
        if !explicit.is_empty() {
            explicit.trim_end_matches('/').to_owned()
        } else if !default_url.is_empty() {
            default_url
        } else {
            return Err("no base URL provided".to_owned());
        }
    } else if !default_url.is_empty() {
        default_url
    } else {
        return Err("unknown provider".to_owned());
    };

    // Determine auth style — providers that support api_type (custom,
    // codingplan, doubao when api_type is set) use the api_type to pick
    // auth; others use the provider's built-in default.
    let auth_style = if supports_api_type && req.api_type.is_some() {
        match effective_type {
            "anthropic" => "x-api-key",
            "gemini" => "gemini-key",
            "ollama" => "none",
            _ => {
                if api_key.is_empty() {
                    "none"
                } else {
                    "bearer"
                }
            }
        }
    } else if effective_type == "gemini" {
        "gemini-key"
    } else {
        default_auth
    };

    // Build models URL — Gemini needs ?key= query param.
    //
    // Critical: route by `effective_type` (the resolved api protocol),
    // not `req.provider`. doubao+anthropic must hit Anthropic's listing
    // path; doubao+openai-completions must hit /v1/models. Using
    // `req.provider` here would force `models_url("doubao", ...)` for
    // every doubao request and 404 on CodingPlan endpoints that only
    // expose `/messages` / `/chat/completions`.
    let is_ollama = effective_type == "ollama";
    let is_gemini = effective_type == "gemini";
    let url = if is_ollama {
        prov_defaults::models_url("ollama", &base_url)
    } else if is_gemini {
        let trimmed = base_url.trim_end_matches('/');
        format!("{trimmed}/models?key={}", api_key)
    } else {
        prov_defaults::models_url(effective_type, &base_url)
    };

    let mut request = client.get(&url);
    match auth_style {
        "bearer" => {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        "x-api-key" => {
            // Send BOTH headers so the same code path covers:
            //   - Standard Anthropic (api.anthropic.com) — uses x-api-key
            //   - Volcengine "coding plan" Anthropic-compat
            //     (ark.cn-beijing.volces.com/api/coding/v1) — uses Bearer
            // Each backend honors its expected header and ignores the other.
            request = request
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("Authorization", format!("Bearer {}", api_key));
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
        models
            .iter()
            .filter_map(|m| {
                m.get("name")
                    .or_else(|| m.get("id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.strip_prefix("models/").unwrap_or(s).to_owned())
            })
            .collect()
    } else {
        vec![]
    }
}

// ---------------------------------------------------------------------------
// Model health (chain failover state)
// ---------------------------------------------------------------------------

/// GET /api/v1/models/health
///
/// Returns the per-model health snapshot for every model the gateway has
/// observed in a failover chain. Drives the UI's "● green / ● yellow /
/// ● red" status dots next to each model id in `agents.defaults.model.*`.
async fn models_health(State(state): State<AppState>) -> Response {
    let models = state.model_health.snapshot();
    Json(serde_json::json!({ "models": models })).into_response()
}

#[derive(Debug, serde::Deserialize)]
struct ModelHealthResetRequest {
    model: String,
}

/// POST /api/v1/models/health/reset
///
/// Manually clear a model's Disabled/Cooling state — used after the
/// operator has fixed the underlying problem (recharged the doubao
/// balance, rotated the OpenAI key, etc.). Returns 404 when the model
/// id isn't known to the health table (typo or never called yet).
async fn models_health_reset(
    State(state): State<AppState>,
    Json(req): Json<ModelHealthResetRequest>,
) -> Response {
    if state.model_health.reset(&req.model) {
        Json(serde_json::json!({ "ok": true, "reset": req.model })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "ok": false,
                "error": "model not found in health table",
            })),
        )
            .into_response()
    }
}

/// Protocol-aware fallback probe used when GET `/models` 404s. Posts a
/// minimal request to the inference endpoint that matches the resolved
/// api protocol; 2xx/400/422 → endpoint reachable (auth + URL fine,
/// request shape rejected is acceptable). Returns `Ok(false)` on
/// 401/403/404/5xx and `Err` on transport failure.
///
/// Duplicates a small slice of base_url/auth resolution from
/// `build_provider_models_request` to keep this isolated; the original
/// function still owns the GET `/models` path.
async fn probe_inference_for_request(
    client: &reqwest::Client,
    req: &TestProviderRequest,
) -> Result<bool, reqwest::Error> {
    use rsclaw_provider::defaults as prov_defaults;

    let api_key = rsclaw_config::loader::expand_env_vars(&req.api_key);
    let base_url_in: Option<String> = req
        .base_url
        .as_deref()
        .map(rsclaw_config::loader::expand_env_vars);

    let is_custom_like = req.provider == "custom" || req.provider == "codingplan";
    let supports_api_type = is_custom_like || req.provider == "doubao";
    let effective_type = if supports_api_type && req.api_type.is_some() {
        req.api_type.as_deref().unwrap_or("openai")
    } else {
        req.provider.as_str()
    };

    let (default_url, _) = if is_custom_like {
        let at = req.api_type.as_deref().unwrap_or("openai");
        prov_defaults::resolve_base_url(at)
    } else {
        prov_defaults::resolve_base_url(&req.provider)
    };

    let base = base_url_in.filter(|u| !u.is_empty()).unwrap_or(default_url);
    if base.is_empty() {
        // Nothing to probe against — caller already failed.
        return Ok(false);
    }
    let base = base.trim_end_matches('/').to_owned();

    let (url, body, auth) = match effective_type {
        "openai-responses" => (
            format!("{base}/responses"),
            serde_json::json!({"model": "test", "input": "hi", "max_output_tokens": 1, "stream": false}),
            "bearer",
        ),
        "anthropic" | "anthropic-messages" => {
            // CodingPlan sits at `/api/coding/v1`; standard Anthropic at
            // `/v1`. If base already has `/v1`, append `/messages`.
            let url = if base.ends_with("/v1") || base.contains("/v1/") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            };
            (
                url,
                serde_json::json!({"model": "test", "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]}),
                "x-api-key",
            )
        }
        "ollama" => {
            // ollama can't 404 on /models (different listing path was
            // already used) — fallback is meaningless here.
            return Ok(false);
        }
        "gemini" => {
            // Gemini uses query-param auth, no inference probe needed.
            return Ok(false);
        }
        _ => (
            format!("{base}/chat/completions"),
            serde_json::json!({"model": "test", "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}], "stream": false}),
            "bearer",
        ),
    };

    let mut request = client.post(&url).json(&body);
    if !api_key.is_empty() {
        match auth {
            "x-api-key" => {
                request = request
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("Authorization", format!("Bearer {api_key}"));
            }
            _ => {
                request = request.header("Authorization", format!("Bearer {api_key}"));
            }
        }
    }

    let resp = request.send().await?;
    let code = resp.status().as_u16();
    Ok(resp.status().is_success() || code == 400 || code == 422)
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
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e})),
            )
                .into_response();
        }
    };

    match request.send().await {
        Ok(resp) if resp.status().is_success() => {
            Json(serde_json::json!({"ok": true, "status": resp.status().as_u16()})).into_response()
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            // Protocol-aware fallback: ARK CodingPlan endpoints
            // (`/api/coding/v3`, `/api/coding/v1`) expose only the
            // inference path and 404 on `/models`. Confirm reachability
            // with a minimal POST probe instead of declaring the key bad.
            if status == 404
                && let Ok(true) = probe_inference_for_request(&client, &req).await
            {
                return Json(serde_json::json!({"ok": true, "status": 200, "fallback": "probe"}))
                    .into_response();
            }
            let body = resp.text().await.unwrap_or_default();
            (StatusCode::OK, Json(serde_json::json!({
                "ok": false,
                "status": status,
                "error": if status == 401 || status == 403 { "Invalid API key" } else { "Request failed" },
                "detail": body.chars().take(200).collect::<String>(),
            }))).into_response()
        }
        Err(e) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": false,
                "error": e.to_string(),
            })),
        )
            .into_response(),
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
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"models": [], "error": e})),
            )
                .into_response();
        }
    };

    match request.send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let models = extract_model_ids(&body);
            Json(serde_json::json!({"models": models})).into_response()
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            // Same fallback as `test_provider`: 404 from `/models` on an
            // endpoint that only has the inference route → POST probe
            // and return empty models. UI displays the MODELS preset.
            if status == 404
                && let Ok(true) = probe_inference_for_request(&client, &req).await
            {
                return Json(serde_json::json!({"models": [], "fallback": "probe"}))
                    .into_response();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({"models": [], "error": format!("HTTP {status}")})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::OK,
            Json(serde_json::json!({"models": [], "error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// WeChat QR code login
// ---------------------------------------------------------------------------

/// POST /api/v1/channels/wechat/qr-login
/// Start WeChat QR login, returns qrcode URL and session token.
/// Uses the silent variant: the HTTP caller (web UI) renders the QR itself
/// from `qrcode_url`, so terminal rendering would only be noise.
async fn wechat_qr_start() -> Response {
    let client = reqwest::Client::new();
    match rsclaw_channel::wechat::WeChatPersonalChannel::start_qr_login_silent(&client).await {
        Ok((qrcode_url, qrcode_token)) => Json(serde_json::json!({
            "qrcode_url": qrcode_url,
            "qrcode_token": qrcode_token,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
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
    match rsclaw_channel::wechat::WeChatPersonalChannel::poll_qr_status(&client, &req.qrcode_token)
        .await
    {
        Ok(Some((bot_token, bot_id))) => Json(serde_json::json!({
            "status": "ok",
            "bot_token": bot_token,
            "bot_id": bot_id,
        }))
        .into_response(),
        Ok(None) => Json(serde_json::json!({
            "status": "waiting",
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Doctor (diagnostic check + auto-fix)
// ---------------------------------------------------------------------------

/// Run `rsclaw doctor` and return structured output.
/// GET /api/v1/defaults — return the provider / channel / search-engine
/// catalog parsed from `defaults.toml` (user file with embedded fallback).
///
/// The JSON shape is the serialization of [`rsclaw_config::catalog::Catalog`]:
/// `{ "providers": [...], "channels": [...], "search_engines": [...] }` with
/// **snake_case** field names throughout (e.g. `name_zh`, `key_placeholder`,
/// `has_base_url`). Channel field keys (`appId`, …) pass through as authored.
async fn get_defaults() -> Response {
    match rsclaw_config::catalog::load_catalog() {
        Ok(catalog) => (StatusCode::OK, Json(serde_json::json!(catalog))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to parse defaults.toml: {e}")})),
        )
            .into_response(),
    }
}

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
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
    }

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
                } else if let Some(msg) = clean
                    .strip_prefix("[error]")
                    .or_else(|| clean.strip_prefix("[err]"))
                {
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
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
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
/// Read the last N lines from the gateway log file, parse into structured
/// entries.
async fn get_logs(Query(q): Query<LogsQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).min(200);
    let log_path = rsclaw_config::loader::log_file();

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
                    let utc = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                        naive,
                        chrono::Utc,
                    );
                    utc.with_timezone(&chrono::Local)
                        .format("%H:%M:%S")
                        .to_string()
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
    let base = rsclaw_config::loader::base_dir();
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
async fn list_workspace_files(Query(q): Query<WorkspaceQuery>) -> Response {
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
    Json(serde_json::json!({ "files": files, "workspace": ws.display().to_string() }))
        .into_response()
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
        )
            .into_response();
    }
    let full_path = ws.join(file_name);
    match std::fs::read_to_string(&full_path) {
        Ok(content) => Json(serde_json::json!({
            "file": file_name,
            "content": content,
        }))
        .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "file not found"})),
        )
            .into_response(),
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
        )
            .into_response();
    }
    // Create workspace dir if it doesn't exist.
    if !ws.exists() {
        if let Err(e) = std::fs::create_dir_all(&ws) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }
    let full_path = ws.join(file_name);
    match std::fs::write(&full_path, &req.content) {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "file": file_name,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// OpenAI Files API
// ---------------------------------------------------------------------------

/// Maximum file upload size (100 MB). TODO: make configurable via
/// gateway.max_upload_size.
const MAX_UPLOAD_SIZE: usize = 100 * 1024 * 1024;

/// Validate a file_id to prevent path traversal attacks.
fn validate_file_id(file_id: &str) -> Result<(), Response> {
    if !file_id.starts_with("file-")
        || file_id.contains('/')
        || file_id.contains('\\')
        || file_id.contains("..")
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid file_id"})),
        )
            .into_response());
    }
    Ok(())
}

/// Directory where uploaded files are stored. Routes through
/// `config::loader::base_dir()` so `--dev` / `--profile` / `RSCLAW_BASE_DIR`
/// land uploads in the matching profile dir, not always `~/.rsclaw/`.
fn files_dir() -> std::path::PathBuf {
    rsclaw_config::loader::base_dir().join("var/data/files")
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
    match filename
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
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
async fn upload_file(State(state): State<AppState>, mut multipart: Multipart) -> impl IntoResponse {
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
                let filename = field.file_name().unwrap_or("upload").to_string();
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
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "error": "file too large, max 100MB"
            })),
        )
            .into_response();
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
    if let Err(e) = validate_file_id(&file_id) {
        return e;
    }
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
    if let Err(e) = validate_file_id(&file_id) {
        return e;
    }
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

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
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
    if let Err(e) = validate_file_id(&file_id) {
        return e.into_response();
    }
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
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Memory management API
// ---------------------------------------------------------------------------
//
// Read-only browse endpoints for the desktop's Memory page. Both
// handlers open `MemoryStore` in read-only mode for the duration of
// the request — the agent runtime keeps its own write-capable
// instance and we don't share state. redb allows multiple readers
// alongside one writer, so this is safe.

#[derive(Debug, Deserialize)]
struct MemoryListParams {
    /// Semantic search query. When set, returns the top-K matches
    /// instead of iterating active docs.
    q: Option<String>,
    /// Filter by exact `scope` (agent id or "global").
    scope: Option<String>,
    /// Filter by exact `kind` ("note" | "fact" | "summary" | "session" | ...).
    kind: Option<String>,
    /// Max results (defaults to 200, hard cap 1000).
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct MemoryDocOut {
    id: String,
    scope: String,
    kind: String,
    text: String,
    abstract_text: Option<String>,
    overview_text: Option<String>,
    tags: Vec<String>,
    tier: String,
    importance: f32,
    pinned: bool,
    created_at: i64,
    accessed_at: i64,
    access_count: i64,
    /// Weibull stretched-exponential decay score (0..1). Computed
    /// on read since it depends on `now()`.
    relevance_score: f32,
}

impl From<&rsclaw_agent::memory::MemoryDoc> for MemoryDocOut {
    fn from(d: &rsclaw_agent::memory::MemoryDoc) -> Self {
        let tier = match d.tier {
            rsclaw_agent::memory::MemDocTier::Core => "core",
            rsclaw_agent::memory::MemDocTier::Working => "working",
            rsclaw_agent::memory::MemDocTier::Peripheral => "peripheral",
        }
        .to_string();
        Self {
            id: d.id.clone(),
            scope: d.scope.clone(),
            kind: d.kind.clone(),
            text: d.text.clone(),
            abstract_text: d.abstract_text.clone(),
            overview_text: d.overview_text.clone(),
            tags: d.tags.clone(),
            tier,
            importance: d.importance,
            pinned: d.pinned,
            created_at: d.created_at,
            accessed_at: d.accessed_at,
            access_count: d.access_count,
            relevance_score: d.relevance_score(),
        }
    }
}

#[derive(Debug, Serialize)]
struct MemoryListResponse {
    docs: Vec<MemoryDocOut>,
    /// Total before `limit` is applied (so the UI can show "X of Y").
    total: usize,
}

/// The gateway already holds the memory store open with redb's exclusive
/// write lock. A second `MemoryStore::open_readonly` on the same file fails
/// ("open memory redb (readonly)"), so HTTP handlers reuse the live
/// gateway-wide store from `AppState` instead of opening their own.
fn live_memory_store(
    state: &AppState,
) -> Option<std::sync::Arc<tokio::sync::Mutex<rsclaw_agent::memory::MemoryStore>>> {
    state.memory.clone()
}

async fn memory_list_docs(
    State(state): State<AppState>,
    Query(params): Query<MemoryListParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(200).min(1000);

    let Some(mem) = live_memory_store(&state) else {
        // Memory disabled or no agent — show "no memories yet" gracefully.
        return Json(MemoryListResponse {
            docs: vec![],
            total: 0,
        })
        .into_response();
    };
    let mut store = mem.lock().await;

    let docs: Vec<rsclaw_agent::memory::MemoryDoc> =
        if let Some(q) = params.q.as_deref().filter(|s| !s.trim().is_empty()) {
            match store.search(q, None, limit).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, query = %q, "memory_list: search failed");
                    vec![]
                }
            }
        } else {
            store.list_active()
        };
    drop(store);

    let filtered: Vec<&rsclaw_agent::memory::MemoryDoc> = docs
        .iter()
        .filter(|d| params.scope.as_ref().is_none_or(|s| &d.scope == s))
        .filter(|d| params.kind.as_ref().is_none_or(|k| &d.kind == k))
        .collect();

    let total = filtered.len();
    let truncated: Vec<MemoryDocOut> = filtered.iter().take(limit).map(|d| (*d).into()).collect();

    Json(MemoryListResponse {
        docs: truncated,
        total,
    })
    .into_response()
}

#[derive(Debug, Serialize)]
struct MemoryStatsResponse {
    /// Total active docs (post-tombstone filter).
    total: usize,
    /// Counts by tier ("core" / "working" / "peripheral").
    by_tier: std::collections::HashMap<String, usize>,
    /// Counts by `kind` field ("note" / "fact" / "summary" / "session" / ...).
    by_kind: std::collections::HashMap<String, usize>,
    /// Counts by `scope` field (agent id or "global").
    by_scope: std::collections::HashMap<String, usize>,
    /// Number of pinned docs.
    pinned: usize,
}

async fn memory_stats(State(state): State<AppState>) -> impl IntoResponse {
    let Some(mem) = live_memory_store(&state) else {
        return Json(MemoryStatsResponse {
            total: 0,
            by_tier: Default::default(),
            by_kind: Default::default(),
            by_scope: Default::default(),
            pinned: 0,
        })
        .into_response();
    };
    let store = mem.lock().await;

    let docs = store.list_active();
    let mut by_tier: std::collections::HashMap<String, usize> = Default::default();
    let mut by_kind: std::collections::HashMap<String, usize> = Default::default();
    let mut by_scope: std::collections::HashMap<String, usize> = Default::default();
    let mut pinned = 0usize;
    for d in &docs {
        let tier = match d.tier {
            rsclaw_agent::memory::MemDocTier::Core => "core",
            rsclaw_agent::memory::MemDocTier::Working => "working",
            rsclaw_agent::memory::MemDocTier::Peripheral => "peripheral",
        };
        *by_tier.entry(tier.to_string()).or_default() += 1;
        *by_kind.entry(d.kind.clone()).or_default() += 1;
        *by_scope.entry(d.scope.clone()).or_default() += 1;
        if d.pinned {
            pinned += 1;
        }
    }

    Json(MemoryStatsResponse {
        total: docs.len(),
        by_tier,
        by_kind,
        by_scope,
        pinned,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct MemoryAddRequest {
    text: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    importance: Option<f32>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    pinned: Option<bool>,
}

#[derive(Debug, Serialize)]
struct MemoryAddResponse {
    id: String,
    scope: String,
    kind: String,
    tier: String,
    deduped: bool,
}

/// POST /api/v1/memory/docs — write a memory entry.
///
/// Inserts via the same `add_off_lock` path the agent runtime uses, so
/// embeddings are computed off the store mutex and the new doc is
/// immediately discoverable by subsequent searches. If an identical
/// (scope, kind, text) tuple already exists, returns that doc's id and
/// flags `deduped: true` instead of creating a duplicate.
async fn memory_add_doc(
    State(state): State<AppState>,
    Json(req): Json<MemoryAddRequest>,
) -> impl IntoResponse {
    let text = req.text.trim().to_owned();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "text required"})),
        )
            .into_response();
    }
    let Some(mem) = live_memory_store(&state) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "memory store not available"})),
        )
            .into_response();
    };
    let scope = req
        .scope
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "global".to_owned());
    let kind = req
        .kind
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "fact".to_owned());
    let importance = req.importance.unwrap_or(0.7).clamp(0.0, 1.0);
    let tags = req.tags.unwrap_or_default();
    let pinned = req.pinned.unwrap_or(false);

    // Dedupe lookup BEFORE generating an id so an HTTP caller hammering
    // the endpoint with the same fact doesn't keep creating new ids.
    if let Some(existing) = {
        let store = mem.lock().await;
        store
            .find_exact(&scope, &kind, &text)
            .map(|d| MemoryAddResponse {
                id: d.id.clone(),
                scope: d.scope.clone(),
                kind: d.kind.clone(),
                tier: match d.tier {
                    rsclaw_agent::memory::MemDocTier::Core => "core",
                    rsclaw_agent::memory::MemDocTier::Working => "working",
                    rsclaw_agent::memory::MemDocTier::Peripheral => "peripheral",
                }
                .to_string(),
                deduped: true,
            })
    } {
        return Json(existing).into_response();
    }

    let id = uuid::Uuid::new_v4().to_string();
    let doc = rsclaw_agent::memory::MemoryDoc {
        id: id.clone(),
        scope: scope.clone(),
        kind: kind.clone(),
        text,
        vector: vec![],
        created_at: 0,
        accessed_at: 0,
        access_count: 0,
        importance,
        tier: Default::default(),
        abstract_text: None,
        overview_text: None,
        tags,
        pinned,
    };
    if let Err(e) = rsclaw_agent::memory::add_off_lock(&mem, doc).await {
        warn!(error = %e, "memory_add: store insert failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e:#}")})),
        )
            .into_response();
    }

    // Re-read to surface the tier the store actually evaluated this doc
    // into (Core/Working/Peripheral depends on importance + thresholds).
    let tier_str = {
        let store = mem.lock().await;
        store
            .get_sync(&id)
            .map(|d| match d.tier {
                rsclaw_agent::memory::MemDocTier::Core => "core",
                rsclaw_agent::memory::MemDocTier::Working => "working",
                rsclaw_agent::memory::MemDocTier::Peripheral => "peripheral",
            })
            .unwrap_or("working")
            .to_string()
    };

    Json(MemoryAddResponse {
        id,
        scope,
        kind,
        tier: tier_str,
        deduped: false,
    })
    .into_response()
}
