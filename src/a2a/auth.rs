//! A2A auth middleware — accepts bearer token OR `X-API-Key` header.
//!
//! Tokens come from environment variables:
//!   RSCLAW_A2A_BEARER_TOKENS  — comma-separated list of accepted bearer tokens
//!   RSCLAW_A2A_API_KEYS       — comma-separated list of accepted API keys
//!
//! When BOTH variables are empty/unset, the middleware passes everything
//! through (dev mode). When either is set, at least one of the credentials
//! presented must match for the request to proceed.

use axum::{extract::Request, http::StatusCode, middleware::Next, response::Response};
use std::sync::LazyLock;

static BEARER_TOKENS: LazyLock<Vec<String>> = LazyLock::new(|| split_env("RSCLAW_A2A_BEARER_TOKENS"));
static API_KEYS: LazyLock<Vec<String>> = LazyLock::new(|| split_env("RSCLAW_A2A_API_KEYS"));

fn split_env(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

pub async fn a2a_auth_layer(req: Request, next: Next) -> Result<Response, StatusCode> {
    if BEARER_TOKENS.is_empty() && API_KEYS.is_empty() {
        return Ok(next.run(req).await);
    }
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if let Some(token) = bearer
        && BEARER_TOKENS.iter().any(|t| t == token)
    {
        return Ok(next.run(req).await);
    }
    let api_key = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());
    if let Some(key) = api_key
        && API_KEYS.iter().any(|k| k == key)
    {
        return Ok(next.run(req).await);
    }
    Err(StatusCode::UNAUTHORIZED)
}
