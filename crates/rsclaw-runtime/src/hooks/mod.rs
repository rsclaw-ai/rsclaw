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
//! Auth: the `X-Hook-Token` header (or `Authorization: Bearer <token>`) must
//! match `hooks.token`. A token is mandatory: without one (or when the
//! configured ref does not resolve to a non-empty value) every request is
//! refused with 503. Custom webhook channels (`channels.custom[]` with
//! `type: "webhook"`) have no secret field of their own and are protected by
//! the same `hooks.token`.
//! Session key: `hooks.mappings[].session_key` or `webhook:<path>` default.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use rsclaw_agent::AgentMessage;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::server::{AppState, constant_time_eq};

// ---------------------------------------------------------------------------
// Token check
// ---------------------------------------------------------------------------

/// Why a webhook request failed authentication.
#[derive(Debug, PartialEq, Eq)]
enum HookAuthError {
    /// No usable token is configured; webhooks are disabled until one is.
    NotConfigured,
    /// The caller's token is missing or wrong.
    Invalid,
}

/// Resolve the configured `hooks.token`. Returns `None` when it is absent,
/// unresolvable (missing env var, failed file/exec ref) or blank.
pub fn resolve_hooks_token(config: &rsclaw_config::runtime::RuntimeConfig) -> Option<String> {
    config
        .ops
        .hooks
        .as_ref()
        .and_then(|h| h.token.as_ref())
        .and_then(|t| t.resolve_full(config.ops.secrets.as_ref()))
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

/// Extract the caller-supplied token from `X-Hook-Token` or
/// `Authorization: Bearer <token>`.
fn provided_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-hook-token").and_then(|v| v.to_str().ok()) {
        return Some(v.trim().to_owned());
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().strip_prefix("Bearer "))
        .map(|v| v.trim().to_owned())
}

/// Constant-time check of the request token against the resolved expected
/// token. An empty expected or provided token never matches.
fn check_token(expected: Option<&str>, headers: &HeaderMap) -> Result<(), HookAuthError> {
    let expected = match expected {
        Some(e) if !e.is_empty() => e,
        _ => return Err(HookAuthError::NotConfigured),
    };
    match provided_token(headers) {
        Some(t) if !t.is_empty() && constant_time_eq(&t, expected) => Ok(()),
        _ => Err(HookAuthError::Invalid),
    }
}

/// Map an auth failure to an HTTP response.
fn auth_error_response(path: &str, err: HookAuthError) -> axum::response::Response {
    match err {
        HookAuthError::NotConfigured => {
            warn!(path = %path, "webhook refused: hooks.token is not configured or did not resolve");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "webhook token not configured"})),
            )
                .into_response()
        }
        HookAuthError::Invalid => {
            warn!(path = %path, "webhook rejected: invalid token");
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "invalid token"})),
            )
                .into_response()
        }
    }
}

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

/// Handle `POST /hooks/:path` — custom webhook channels first, then
/// `hooks.mappings`. Every request must carry the `hooks.token`.
pub async fn handle_webhook(
    State(state): State<AppState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Custom webhook channels take priority (don't need hooks.enabled).
    // Clone the Arc out of the lock before .await to avoid holding a
    // !Send RwLockReadGuard across the yield point.
    let custom_ch = {
        let map = state
            .custom_webhooks
            .read()
            .expect("custom_webhooks lock poisoned");
        map.get(path.as_str()).cloned()
    };
    if let Some(ch) = custom_ch {
        // A custom channel named like a local entry point would inherit
        // owner trust; refuse it outright.
        if rsclaw_agent::trust::is_local_channel(path.as_str()) {
            warn!(path = %path, "webhook refused: custom channel uses a reserved local channel name");
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "reserved channel name"})),
            )
                .into_response();
        }
        let expected = resolve_hooks_token(&state.config);
        if let Err(e) = check_token(expected.as_deref(), &headers) {
            return auth_error_response(&path, e);
        }
        // ACK immediately and process the payload in the background.
        // handle_webhook downloads remote attachments and runs the agent
        // pipeline; awaiting it here would hold the HTTP request open for
        // tens of seconds and let slow attachment URLs delay the ACK.
        let body_owned = body.to_vec();
        tokio::spawn(async move {
            let body_str = String::from_utf8_lossy(&body_owned).into_owned();
            ch.handle_webhook(&body_str).await;
        });
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"accepted": true, "channel": path})),
        )
            .into_response();
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

    // Token validation (mandatory).
    let expected = resolve_hooks_token(&state.config);
    if let Err(e) = check_token(expected.as_deref(), &headers) {
        return auth_error_response(&path, e);
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
        trust: rsclaw_agent::SenderTrust::User,
        session_key: session_key.clone(),
        text: message_text,
        channel: format!("webhook:{path}"),
        peer_id: format!("webhook:{path}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            value.parse().expect("header value"),
        );
        h
    }

    #[test]
    fn hooks_token_check_rejects_empty_and_missing() {
        // No expected token: always refused, even with an empty header.
        assert_eq!(
            check_token(None, &headers("x-hook-token", "")),
            Err(HookAuthError::NotConfigured)
        );
        assert_eq!(
            check_token(Some(""), &headers("authorization", "Bearer ")),
            Err(HookAuthError::NotConfigured)
        );
        // Empty / wrong provided tokens are refused.
        assert_eq!(
            check_token(Some("s3cret"), &headers("x-hook-token", "")),
            Err(HookAuthError::Invalid)
        );
        assert_eq!(
            check_token(Some("s3cret"), &headers("authorization", "Bearer ")),
            Err(HookAuthError::Invalid)
        );
        assert_eq!(
            check_token(Some("s3cret"), &HeaderMap::new()),
            Err(HookAuthError::Invalid)
        );
        assert_eq!(
            check_token(Some("s3cret"), &headers("x-hook-token", "nope")),
            Err(HookAuthError::Invalid)
        );
        // Correct tokens pass.
        assert_eq!(
            check_token(Some("s3cret"), &headers("x-hook-token", "s3cret")),
            Ok(())
        );
        assert_eq!(
            check_token(Some("s3cret"), &headers("authorization", "Bearer s3cret")),
            Ok(())
        );
    }
}
