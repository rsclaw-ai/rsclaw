//! HTTP helpers for the rsclaw gen domain (`/v1/images`, `/v1/videos`,
//! `/v1/audio`).
//!
//! Same 307/308 + Bearer-reattach protocol as the LLM-side
//! [`super::rsclaw::RsclawProvider::send_following_redirects`], but
//! standalone — no redirect cache (gen calls are sparse compared to
//! LLM turns; pay the LB hop per request). Shared by:
//!
//! - `src/agent/tools_image.rs` — POST `/v1/images/generations`
//! - `src/agent/tools_video.rs` — POST `/v1/videos`
//! - `src/gateway/external_jobs_worker.rs` — GET `/v1/videos/{id}` and
//!   resolving `/content` → R2 presigned URL
//!
//! reqwest's default redirect policy strips `Authorization` on
//! cross-origin redirects, so we disable auto-follow and manage the
//! loop here, re-attaching Bearer on each hop. The terminal hop that
//! reaches an out-of-fleet presigned URL (e.g. Cloudflare R2 GET) is
//! resolved separately by [`get_content_url`] which RETURNS the
//! redirect target instead of following it — the caller fetches those
//! bytes without auth.

use anyhow::{Result, anyhow};
use reqwest::{Client, Method, Response, StatusCode, Url};
use serde_json::Value;

/// Maximum 307/308 hops we'll follow before bailing. A healthy rsclaw
/// fleet should land in ≤1 hop (LB → backend pool); 5 is generous and
/// avoids burning forever on a misconfigured LB returning 308 in a loop.
const MAX_HOPS: u8 = 5;

/// Hardcoded default when the caller doesn't pass a configured base.
/// Mirrors `https://api.rsclaw.ai` — the public gen surface, NOT the
/// LLM `/v1/agent` mount.
pub const DEFAULT_GEN_HOST: &str = "https://api.rsclaw.ai";

/// Resolve the canonical rsclaw gen host root from caller-supplied
/// config.
///
/// The LLM provider config in `defaults.toml` sets
/// `base_url = "https://api.rsclaw.ai/v1/agent"`. The gen surface
/// lives off the host root (`/v1/images/generations`, `/v1/videos`).
/// This strips the trailing `/v1/agent` (or bare `/v1`) so a single
/// `models.providers.rsclaw.base_url` value powers both surfaces.
///
/// Returns the host with **no trailing slash** and **no version
/// suffix**. Callers append `/v1/<resource>` themselves.
pub fn gen_host_base(configured: Option<&str>) -> String {
    let raw = configured
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_GEN_HOST)
        .trim_end_matches('/');
    if let Some(s) = raw
        .strip_suffix("/v1/agent")
        .or_else(|| raw.strip_suffix("/v1"))
    {
        s.trim_end_matches('/').to_owned()
    } else {
        raw.to_owned()
    }
}

/// Build the per-call redirect-disabled `reqwest::Client`. The shared
/// agent-wide client follows redirects by default (which drops our
/// auth header on cross-origin hops), so each rsclaw call uses a fresh
/// child client.
pub fn build_client(user_agent: &str, timeout_secs: u64) -> Result<Client> {
    Client::builder()
        .user_agent(user_agent)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| anyhow!("rsclaw_http: client build: {e}"))
}

/// POST JSON with Bearer auth; follow 307/308 manually re-attaching
/// the header on each hop. Returns the final non-redirect response
/// for the caller to drain.
pub async fn post_json(
    client: &Client,
    url: &str,
    bearer: &str,
    body: &Value,
) -> Result<Response> {
    send_following(client, Method::POST, url, bearer, Some(body)).await
}

/// GET with Bearer auth; same 307/308 + auth-reattach loop. Use for
/// rsclaw-API GETs that stay inside the fleet (e.g. `/v1/videos/{id}`).
/// For the `/content` → R2 case prefer [`get_content_url`] which
/// resolves the FINAL hop's target URL and hands it back so the caller
/// can fetch unauthenticated.
pub async fn get(client: &Client, url: &str, bearer: &str) -> Result<Response> {
    send_following(client, Method::GET, url, bearer, None).await
}

/// GET an endpoint expected to return a single 307 redirect to an
/// out-of-fleet presigned URL (currently only `/v1/videos/{id}/content`
/// → Cloudflare R2 presigned GET).
///
/// Returns:
/// - `Ok(Some(target_url))` — the 307 Location, fetchable without auth
/// - `Ok(None)` — endpoint returned 2xx with bytes inline (dev/in-mem
///   BlobStore path; caller can fall back to `get()` + bytes())
/// - `Err(_)` — non-redirect non-success status, or missing Location
///
/// We deliberately do NOT auto-follow into the presigned target — the
/// download path in `external_jobs_worker::download_artifact` is
/// authless and takes the URL directly, so the auth header never
/// leaks to Cloudflare.
pub async fn get_content_url(client: &Client, url: &str, bearer: &str) -> Result<Option<String>> {
    let resp = client
        .get(url)
        .bearer_auth(bearer)
        .send()
        .await
        .map_err(|e| anyhow!("rsclaw_http: GET {url}: {e}"))?;
    let st = resp.status();
    if st == StatusCode::TEMPORARY_REDIRECT || st == StatusCode::PERMANENT_REDIRECT {
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("rsclaw_http: {st} omitted Location"))?;
        return Ok(Some(resolve_location(url, loc)?));
    }
    if !st.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "rsclaw_http: GET {url} returned {st}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    Ok(None)
}

async fn send_following(
    client: &Client,
    method: Method,
    initial_url: &str,
    bearer: &str,
    body: Option<&Value>,
) -> Result<Response> {
    let mut current = initial_url.to_owned();
    for _ in 0..=MAX_HOPS {
        let mut builder = client
            .request(method.clone(), &current)
            .bearer_auth(bearer);
        if let Some(b) = body {
            builder = builder.json(b);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| anyhow!("rsclaw_http: {method} {current}: {e}"))?;
        let st = resp.status();
        if st == StatusCode::TEMPORARY_REDIRECT || st == StatusCode::PERMANENT_REDIRECT {
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("rsclaw_http: {st} omitted Location"))?
                .to_owned();
            current = resolve_location(&current, &loc)?;
            continue;
        }
        return Ok(resp);
    }
    Err(anyhow!(
        "rsclaw_http: too many redirects starting from {initial_url}"
    ))
}

fn resolve_location(current: &str, loc: &str) -> Result<String> {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        Ok(loc.to_owned())
    } else {
        Url::parse(current)
            .and_then(|u| u.join(loc))
            .map(|u| u.to_string())
            .map_err(|e| anyhow!("rsclaw_http: resolve Location {loc:?}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen_host_base_strips_v1_agent() {
        assert_eq!(
            gen_host_base(Some("https://api.rsclaw.ai/v1/agent")),
            "https://api.rsclaw.ai"
        );
        assert_eq!(
            gen_host_base(Some("https://api.rsclaw.ai/v1/agent/")),
            "https://api.rsclaw.ai"
        );
    }

    #[test]
    fn gen_host_base_strips_bare_v1() {
        assert_eq!(
            gen_host_base(Some("https://api.rsclaw.ai/v1")),
            "https://api.rsclaw.ai"
        );
    }

    #[test]
    fn gen_host_base_preserves_already_root() {
        assert_eq!(
            gen_host_base(Some("https://api.rsclaw.ai")),
            "https://api.rsclaw.ai"
        );
        assert_eq!(
            gen_host_base(Some("https://api.rsclaw.ai/")),
            "https://api.rsclaw.ai"
        );
    }

    #[test]
    fn gen_host_base_empty_or_none_yields_default() {
        assert_eq!(gen_host_base(None), DEFAULT_GEN_HOST);
        assert_eq!(gen_host_base(Some("")), DEFAULT_GEN_HOST);
    }

    #[test]
    fn gen_host_base_custom_host_self_hosted() {
        assert_eq!(
            gen_host_base(Some("https://gen.internal:8443/v1/agent")),
            "https://gen.internal:8443"
        );
    }

    #[test]
    fn resolve_location_absolute() {
        assert_eq!(
            resolve_location("https://api.rsclaw.ai/v1/videos", "https://backend-a.rsclaw.ai/v1/videos").unwrap(),
            "https://backend-a.rsclaw.ai/v1/videos"
        );
    }

    #[test]
    fn resolve_location_relative_path() {
        assert_eq!(
            resolve_location("https://api.rsclaw.ai/v1/videos/video_abc", "/backend/v1/videos/video_abc").unwrap(),
            "https://api.rsclaw.ai/backend/v1/videos/video_abc"
        );
    }
}
