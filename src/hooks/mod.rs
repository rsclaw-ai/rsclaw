//! Webhook ingress — POST /hooks/:path
//!
//! Maps inbound HTTP webhook calls to agent messages. Routing is configured
//! via the `hooks` section of rsclaw.json5 / openclaw.json:
//!
//! ```json5
//! hooks: {
//!   enabled: true,
//!   token: "${HOOKS_TOKEN}",
//!   path: "/hooks",
//!   mappings: [
//!     { path: "github", agent_id: "devbot", session_key: "webhook:github" },
//!   ],
//! }
//! ```
//!
//! Auth: the `X-Hook-Token` header must match `hooks.token` when set.
//! Session key: `hooks.mappings[].session_key` or `webhook:<path>` default.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::{agent::AgentMessage, server::AppState};

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HookResponse {
    accepted: bool,
    session_key: String,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub async fn handle_webhook(
    State(state): State<AppState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Custom webhook channels take priority (don't need hooks.enabled).
    if let Ok(map) = state.custom_webhooks.read() {
        if let Some(ch) = map.get(path.as_str()) {
            let body_str = String::from_utf8_lossy(&body);
            ch.handle_webhook(&body_str);
            return (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"accepted": true, "channel": path})),
            )
                .into_response();
        }
    }

    let hooks_cfg = match state.config.ops.hooks.as_ref() {
        Some(h) if h.enabled => h,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "webhooks not enabled"})),
            )
                .into_response();
        }
    };

    // Token validation.
    if let Some(ref expected) = hooks_cfg.token {
        let expected_plain = expected.as_plain().unwrap_or("");
        let provided = headers
            .get("x-hook-token")
            .or_else(|| headers.get("authorization"))
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim_start_matches("Bearer ").trim());

        match provided {
            Some(t) if t == expected_plain => {}
            _ => {
                warn!(path = %path, "webhook rejected: invalid token");
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "invalid token"})),
                )
                    .into_response();
            }
        }
    }

    // (Custom webhook channels already checked above, before hooks_cfg gate.)

    // Find mapping.
    let mapping = hooks_cfg.mappings.as_ref().and_then(|m| {
        m.iter()
            .find(|e| e.match_.path.as_deref() == Some(path.as_str()))
    });

    let (agent_id, session_key, message_text) = if let Some(m) = mapping {
        let agent = m.agent_id.clone().unwrap_or_else(|| "main".to_string());
        let sess = m
            .session_key
            .clone()
            .unwrap_or_else(|| format!("webhook:{path}"));
        let text = format!(
            "[webhook path={}]\n{}",
            path,
            String::from_utf8_lossy(&body)
        );
        (agent, sess, text)
    } else {
        // No explicit mapping — route to default agent.
        let sess = format!("webhook:{path}");
        let text = String::from_utf8_lossy(&body).into_owned();
        ("default".to_string(), sess, text)
    };

    // Allow caller to override session key if permitted.
    let session_key = if hooks_cfg.allow_request_session_key.unwrap_or(false) {
        if let Some(override_key) = headers.get("x-session-key").and_then(|v| v.to_str().ok()) {
            let allowed = hooks_cfg
                .allowed_session_key_prefixes
                .as_ref()
                .is_none_or(|prefixes| {
                    prefixes
                        .iter()
                        .any(|p| override_key.starts_with(p.as_str()))
                });
            if allowed {
                override_key.to_string()
            } else {
                warn!("webhook session key override rejected: prefix not allowed");
                session_key
            }
        } else {
            session_key
        }
    } else {
        session_key
    };

    // Resolve agent.
    let handle = match state
        .agents
        .get(&agent_id)
        .or_else(|_| state.agents.default_agent())
    {
        Ok(h) => h,
        Err(e) => {
            warn!(path = %path, "webhook: agent not found: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "agent not found"})),
            )
                .into_response();
        }
    };

    info!(path = %path, agent = %agent_id, session = %session_key, "webhook received");

    // Fire-and-forget: webhooks don't wait for agent reply.
    let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text: message_text,
        channel: format!("webhook:{path}"),
        peer_id: format!("webhook:{path}"),
        chat_id: String::new(),
        reply_tx,
        extra_tools: vec![],
        images: vec![],
        files: vec![],
    };

    if handle.tx.send(msg).await.is_err() {
        debug!(path = %path, "webhook: agent inbox closed");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "agent unavailable"})),
        )
            .into_response();
    }

    (
        StatusCode::ACCEPTED,
        Json(HookResponse {
            accepted: true,
            session_key,
        }),
    )
        .into_response()
}
