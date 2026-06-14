//! Tiny HTTP client helpers for `rsclaw` CLI subcommands that need to
//! talk to a running local gateway instead of opening shared on-disk
//! state directly.
//!
//! Why this module exists: subcommands like `rsclaw memory search` and
//! `rsclaw kb add` were originally written to open redb directly. Once
//! the gateway holds the same database with an exclusive write lock,
//! the CLI's "readonly" open path either fights the lock or silently
//! sees a stale view. The fix is to route through the gateway's HTTP
//! API instead — which is also the only way `rsclaw memory save` /
//! `rsclaw kb add` writes can succeed without stopping the gateway.
//!
//! The functions here intentionally have a small surface: read config
//! once, build a typed `reqwest::Client`, return parsed JSON or a
//! human-friendly error. Callers branch on `is_gateway_up()` when they
//! want a graceful fallback to a direct-open readonly path.

use anyhow::{Context, Result, anyhow};
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;

use rsclaw_config as config;

const DEFAULT_PORT: u16 = 18888;

/// Resolved gateway endpoint config: which port to hit and which bearer
/// token to send. `token` is `None` when the user runs the gateway with
/// `authToken` unset (loopback-only no-auth mode); callers should still
/// build the request without an `Authorization` header in that case.
#[derive(Debug, Clone)]
pub struct GatewayEndpoint {
    pub port: u16,
    pub token: Option<String>,
}

impl GatewayEndpoint {
    pub fn resolve() -> Self {
        let cfg = config::load().ok();
        let port = cfg.as_ref().map_or(DEFAULT_PORT, |c| c.gateway.port);
        let token = cfg.as_ref().and_then(|c| c.gateway.auth_token.clone());
        Self { port, token }
    }

    pub fn url(&self, path: &str) -> String {
        // path must start with "/api/v1/..." — caller's responsibility.
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
}

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(2))
        .build()
        .expect("reqwest client build")
}

fn auth_header<'a>(rb: reqwest::RequestBuilder, token: Option<&'a str>) -> reqwest::RequestBuilder {
    match token {
        Some(t) if !t.is_empty() => rb.header("Authorization", format!("Bearer {t}")),
        _ => rb,
    }
}

/// Probe `/api/v1/health` with a short timeout. Returns `true` only if
/// the gateway answers 2xx — used by subcommands that want to fall back
/// to a direct file open when the gateway is down.
pub async fn is_gateway_up() -> bool {
    let ep = GatewayEndpoint::resolve();
    let c = client(Duration::from_millis(800));
    matches!(
        c.get(ep.url("/api/v1/health")).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// GET the gateway and deserialize JSON. Errors carry the HTTP status and
/// (a slice of) the response body so the CLI's failure messages are
/// actionable instead of "internal error".
pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T> {
    let ep = GatewayEndpoint::resolve();
    let c = client(Duration::from_secs(30));
    let rb = c.get(ep.url(path));
    let resp = auth_header(rb, ep.token.as_deref())
        .send()
        .await
        .with_context(|| format!("gateway GET {path} failed"))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("gateway returned {st}: {}", body.chars().take(400).collect::<String>()));
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("gateway GET {path}: invalid JSON response"))
}

/// POST JSON body and deserialize JSON response. Same error semantics as
/// `get_json`.
pub async fn post_json<B: Serialize, T: DeserializeOwned>(path: &str, body: &B) -> Result<T> {
    let ep = GatewayEndpoint::resolve();
    let c = client(Duration::from_secs(60));
    let rb = c.post(ep.url(path)).json(body);
    let resp = auth_header(rb, ep.token.as_deref())
        .send()
        .await
        .with_context(|| format!("gateway POST {path} failed"))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("gateway returned {st}: {}", body.chars().take(400).collect::<String>()));
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("gateway POST {path}: invalid JSON response"))
}

/// PATCH JSON body. Same error semantics as `get_json`/`post_json`.
pub async fn patch_json<B: Serialize, T: DeserializeOwned>(path: &str, body: &B) -> Result<T> {
    let ep = GatewayEndpoint::resolve();
    let c = client(Duration::from_secs(30));
    let rb = c.patch(ep.url(path)).json(body);
    let resp = auth_header(rb, ep.token.as_deref())
        .send()
        .await
        .with_context(|| format!("gateway PATCH {path} failed"))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("gateway returned {st}: {}", body.chars().take(400).collect::<String>()));
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("gateway PATCH {path}: invalid JSON response"))
}

/// DELETE returning JSON.
pub async fn delete_json<T: DeserializeOwned>(path: &str) -> Result<T> {
    let ep = GatewayEndpoint::resolve();
    let c = client(Duration::from_secs(30));
    let rb = c.delete(ep.url(path));
    let resp = auth_header(rb, ep.token.as_deref())
        .send()
        .await
        .with_context(|| format!("gateway DELETE {path} failed"))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("gateway returned {st}: {}", body.chars().take(400).collect::<String>()));
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("gateway DELETE {path}: invalid JSON response"))
}

/// GET raw bytes (for content endpoints that return non-JSON payloads
/// like markdown). Same auth + error semantics as `get_json`.
pub async fn get_bytes(path: &str) -> Result<Vec<u8>> {
    let ep = GatewayEndpoint::resolve();
    let c = client(Duration::from_secs(60));
    let rb = c.get(ep.url(path));
    let resp = auth_header(rb, ep.token.as_deref())
        .send()
        .await
        .with_context(|| format!("gateway GET {path} failed"))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("gateway returned {st}: {}", body.chars().take(400).collect::<String>()));
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Format a "gateway down" hint suitable for CLI error output. Used by
/// HTTP-only subcommands that can't fall back to a direct file open.
pub fn down_hint() -> String {
    "gateway is not reachable on loopback. start it with: rsclaw gateway start".to_string()
}
