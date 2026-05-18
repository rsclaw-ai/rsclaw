//! A2A auth middleware — accepts bearer token OR `X-API-Key` header.
//!
//! Credentials come from the live gateway config (`gateway.a2aAuth.tokens` /
//! `apiKeys`), plus `gateway.auth.token` (single-token convenience), plus the
//! env vars `RSCLAW_A2A_BEARER_TOKENS` / `RSCLAW_A2A_API_KEYS` for back-compat.
//! Env-set lists were already merged into the runtime config at startup, so
//! the middleware only consults the live config and is reload-safe — any
//! token rotation that updates the live config takes effect immediately,
//! and a process respawn doesn't lose anything (config is on disk).
//!
//! When ALL credential lists are empty AND `gateway.auth.token` is unset,
//! the middleware passes everything through (dev mode).

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};

use crate::server::{AppState, constant_time_eq};

/// Snapshot of accepted A2A credentials, materialised once per request.
struct Accepted {
    tokens: Vec<String>,
    api_keys: Vec<String>,
}

impl Accepted {
    fn is_empty(&self) -> bool {
        self.tokens.is_empty() && self.api_keys.is_empty()
    }

    fn matches_bearer(&self, presented: &str) -> bool {
        self.tokens.iter().any(|t| constant_time_eq(t, presented))
    }

    fn matches_api_key(&self, presented: &str) -> bool {
        self.api_keys.iter().any(|k| constant_time_eq(k, presented))
    }
}

/// Build the per-request accepted-credentials set from live state.
///
/// Tokens come from three places, deduped: gateway.a2aAuth.tokens (config),
/// env (merged at startup into the same Vec), and gateway.auth.token (also
/// accepted as a Bearer so a single config token unifies all gateway auth).
async fn collect_accepted(state: &AppState) -> Accepted {
    let gw = state.live.gateway.read().await;
    let mut tokens: Vec<String> = gw.a2a_bearer_tokens.clone();
    if let Some(unified) = gw.auth_token.as_ref() {
        if !tokens.iter().any(|t| t == unified) {
            tokens.push(unified.clone());
        }
    }
    Accepted {
        tokens,
        api_keys: gw.a2a_api_keys.clone(),
    }
}

pub async fn a2a_auth_layer(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let accepted = collect_accepted(&state).await;
    if accepted.is_empty() {
        // Dev pass-through — nothing configured anywhere.
        return Ok(next.run(req).await);
    }

    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if let Some(token) = bearer
        && accepted.matches_bearer(token)
    {
        return Ok(next.run(req).await);
    }

    let api_key = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());
    if let Some(key) = api_key
        && accepted.matches_api_key(key)
    {
        return Ok(next.run(req).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_accepted(tokens: &[&str], api_keys: &[&str]) -> Accepted {
        Accepted {
            tokens: tokens.iter().map(|s| (*s).to_owned()).collect(),
            api_keys: api_keys.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn empty_accepted_is_dev_passthrough_marker() {
        assert!(make_accepted(&[], &[]).is_empty());
        assert!(!make_accepted(&["x"], &[]).is_empty());
        assert!(!make_accepted(&[], &["y"]).is_empty());
    }

    #[test]
    fn bearer_match_is_constant_time_safe() {
        let a = make_accepted(&["right"], &[]);
        assert!(a.matches_bearer("right"));
        assert!(!a.matches_bearer("wrong"));
        // Length mismatch is rejected by constant_time_eq.
        assert!(!a.matches_bearer("rightandmore"));
    }

    #[test]
    fn api_key_match() {
        let a = make_accepted(&[], &["k1", "k2"]);
        assert!(a.matches_api_key("k1"));
        assert!(a.matches_api_key("k2"));
        assert!(!a.matches_api_key("k3"));
    }

    #[test]
    fn multiple_tokens_all_accepted() {
        let a = make_accepted(&["t1", "t2", "t3"], &[]);
        assert!(a.matches_bearer("t1"));
        assert!(a.matches_bearer("t2"));
        assert!(a.matches_bearer("t3"));
        assert!(!a.matches_bearer("t4"));
    }
}
