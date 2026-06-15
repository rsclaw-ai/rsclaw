//! Shared rsclaw fleet HTTP client with 307/308 redirect caching.
//!
//! `api.rsclaw.ai` fronts the GPU fleet behind a 308-emitting load balancer:
//! the first request to an origin pays the redirect, after which the target
//! origin is cached (per `Cache-Control: max-age`, or [`DEFAULT_REDIRECT_TTL`])
//! so subsequent requests route DIRECT until the entry expires. 307 (temporary)
//! is followed but never cached. This is the SINGLE implementation shared by the
//! rsclaw provider AND the fleet capability clients (OCR / embed / rerank) so
//! they all amortise the LB cost instead of re-paying the redirect per call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use reqwest::StatusCode;
use serde::Serialize;

/// Maximum redirect hops before bailing — a healthy topology is ONE 308 hop.
const MAX_REDIRECT_HOPS: usize = 5;
/// Fallback redirect-cache TTL when a 308 omits `Cache-Control: max-age`.
const DEFAULT_REDIRECT_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
struct RedirectEntry {
    target_origin: String,
    expires_at: Instant,
}

/// Cache of 308 redirects keyed by *origin* (scheme + host + port).
#[derive(Debug, Default)]
pub struct RedirectCache {
    entries: HashMap<String, RedirectEntry>,
}

impl RedirectCache {
    /// Look up `origin`; returns a fresh target origin, evicting stale rows.
    fn lookup(&mut self, origin: &str) -> Option<String> {
        let now = Instant::now();
        match self.entries.get(origin) {
            Some(e) if e.expires_at > now => Some(e.target_origin.clone()),
            Some(_) => {
                self.entries.remove(origin);
                None
            }
            None => None,
        }
    }

    fn store(&mut self, origin: String, target_origin: String, ttl: Duration) {
        self.entries.insert(
            origin,
            RedirectEntry {
                target_origin,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    fn invalidate(&mut self, origin: &str) {
        self.entries.remove(origin);
    }
}

/// Extract the origin (`scheme://host[:port]`) prefix from a URL.
fn origin_of(url: &str) -> Option<&str> {
    let scheme_end = url.find("://")? + 3;
    let path_start = url[scheme_end..]
        .find('/')
        .map(|i| scheme_end + i)
        .unwrap_or(url.len());
    Some(&url[..path_start])
}

/// Rewrite the origin portion of `url` to `new_origin`, keeping path/query.
fn rewrite_origin(url: &str, new_origin: &str) -> String {
    match origin_of(url) {
        Some(orig) => format!("{}{}", new_origin, &url[orig.len()..]),
        None => url.to_owned(),
    }
}

/// Resolve a `Location` header against the current URL (absolute URL or
/// absolute path only — what rsclaw-server actually emits).
fn resolve_location(base: &str, location: &str) -> Option<String> {
    if location.is_empty() {
        return None;
    }
    if location.contains("://") {
        return Some(location.to_owned());
    }
    let base_origin = origin_of(base)?;
    if location.starts_with('/') {
        Some(format!("{base_origin}{location}"))
    } else {
        None
    }
}

/// Parse `Cache-Control: …, max-age=N, …` → seconds, or `None` when missing /
/// malformed / overridden by `no-store` / `no-cache`.
fn parse_max_age(cache_control: Option<&str>) -> Option<Duration> {
    let header = cache_control?;
    let mut max_age: Option<u64> = None;
    for directive in header.split(',') {
        let d_lower = directive.trim().to_ascii_lowercase();
        if d_lower == "no-store" || d_lower == "no-cache" {
            return None;
        }
        if let Some(value) = d_lower.strip_prefix("max-age=") {
            if let Ok(n) = value.trim().parse::<u64>() {
                max_age = Some(n);
            }
        }
    }
    max_age.map(Duration::from_secs)
}

/// One-shot transport retry papering over a half-closed pooled connection on
/// the first request after an idle gap. Genuine 4xx/5xx arrive as `Ok(resp)`.
async fn send_with_transport_retry(
    builder: reqwest::RequestBuilder,
) -> reqwest::Result<reqwest::Response> {
    let retryable = |e: &reqwest::Error| -> bool {
        use std::error::Error;
        if e.is_connect() {
            return true;
        }
        let mut src: Option<&dyn Error> = e.source();
        while let Some(s) = src {
            let msg = s.to_string();
            if msg.contains("closed before message completed")
                || msg.contains("Connection reset")
                || msg.contains("Connection refused")
                || msg.contains("connection closed")
            {
                return true;
            }
            src = s.source();
        }
        false
    };

    let Some(retry_builder) = builder.try_clone() else {
        return builder.send().await;
    };
    match builder.send().await {
        Ok(resp) => Ok(resp),
        Err(e) if retryable(&e) => {
            tokio::time::sleep(Duration::from_millis(200)).await;
            retry_builder.send().await
        }
        Err(e) => Err(e),
    }
}

/// Shared rsclaw fleet HTTP client: reqwest with auto-redirect DISABLED plus an
/// origin-keyed 308 cache, so every fleet caller (provider, OCR, embed, rerank)
/// routes through the LB once and direct thereafter. Cheap to `clone` (the
/// reqwest client and the cache are both `Arc`-backed).
#[derive(Clone)]
pub struct FleetHttp {
    client: reqwest::Client,
    cache: Arc<Mutex<RedirectCache>>,
}

impl FleetHttp {
    /// Build a fleet client. No *total* client timeout (it would kill long
    /// streaming generations); pass a per-call deadline via `builder_timeout`.
    /// Idle-pool reuse is disabled for the same dead-socket reason embeddings
    /// hit; redirects are captured manually so the 308 cache can read headers.
    pub fn new(user_agent: Option<&str>) -> Self {
        let mut b = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(0)
            .connect_timeout(Duration::from_secs(10))
            .tcp_keepalive(Duration::from_secs(30));
        if let Some(ua) = user_agent {
            b = b.user_agent(ua);
        }
        let client = b.build().unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            cache: Arc::new(Mutex::new(RedirectCache::default())),
        }
    }

    /// Wrap a caller-tuned reqwest client (e.g. the rsclaw provider's
    /// connection-reusing client) in the shared 308 cache + redirect loop. The
    /// ONLY requirement is that the client has auto-redirect DISABLED
    /// (`redirect(Policy::none())`) so this loop can read each hop's headers.
    pub fn from_client(client: reqwest::Client) -> Self {
        Self {
            client,
            cache: Arc::new(Mutex::new(RedirectCache::default())),
        }
    }

    /// Rewrite `url`'s origin to a fresh cached 308 target, if any.
    fn resolve_url(&self, url: &str) -> String {
        let Some(origin) = origin_of(url) else {
            return url.to_owned();
        };
        if let Ok(mut cache) = self.cache.lock()
            && let Some(target) = cache.lookup(origin)
        {
            return rewrite_origin(url, &target);
        }
        url.to_owned()
    }

    fn invalidate_redirect_for(&self, url: &str) {
        let Some(origin) = origin_of(url) else { return };
        if let Ok(mut cache) = self.cache.lock() {
            cache.invalidate(origin);
        }
    }

    /// POST `body` to `url`, transparently following 307/308 and caching 308
    /// targets by origin (`Cache-Control: max-age` or [`DEFAULT_REDIRECT_TTL`]).
    /// Non-redirect statuses (incl. errors) are returned as-is for the caller
    /// to interpret. `builder_timeout` is a per-hop deadline (NOT cumulative).
    pub async fn post_following_redirects<B: Serialize>(
        &self,
        url: &str,
        body: &B,
        bearer: Option<&str>,
        accept_sse: bool,
        idempotency_key: Option<&str>,
        builder_timeout: Option<Duration>,
    ) -> Result<reqwest::Response> {
        let mut current_url = self.resolve_url(url);
        let mut hops = 0;
        loop {
            let mut builder = self.client.post(&current_url).json(body);
            if let Some(t) = builder_timeout {
                builder = builder.timeout(t);
            }
            if let Some(key) = idempotency_key {
                builder = builder.header("Idempotency-Key", key);
            }
            if accept_sse {
                builder = builder.header("Accept", "text/event-stream");
            }
            if let Some(k) = bearer.filter(|k| !k.is_empty()) {
                builder = builder.header("authorization", format!("Bearer {k}"));
            }

            let resp = match send_with_transport_retry(builder).await {
                Ok(r) => r,
                Err(e) => {
                    // Transport failure against a redirected target → drop the
                    // cache entry so the next attempt goes back through the LB.
                    self.invalidate_redirect_for(&current_url);
                    return Err(anyhow::Error::from(e).context(format!("rsclaw POST {current_url}")));
                }
            };

            let status = resp.status();
            if status != StatusCode::TEMPORARY_REDIRECT && status != StatusCode::PERMANENT_REDIRECT {
                return Ok(resp);
            }

            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let cache_control = resp
                .headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);

            let Some(loc) = location else {
                anyhow::bail!("rsclaw: {status} redirect from {current_url} omitted Location header");
            };
            let Some(next_url) = resolve_location(&current_url, &loc) else {
                anyhow::bail!(
                    "rsclaw: {status} redirect from {current_url} had unsupported Location {loc:?}"
                );
            };

            // 308 only: cache the target origin. 307 is temporary by spec.
            if status == StatusCode::PERMANENT_REDIRECT
                && let (Some(current_origin), Some(next_origin)) =
                    (origin_of(&current_url), origin_of(&next_url))
                && current_origin != next_origin
            {
                let ttl = parse_max_age(cache_control.as_deref()).unwrap_or(DEFAULT_REDIRECT_TTL);
                if let Ok(mut cache) = self.cache.lock() {
                    cache.store(current_origin.to_owned(), next_origin.to_owned(), ttl);
                }
                tracing::info!(
                    from = %current_origin,
                    to = %next_origin,
                    ttl_secs = ttl.as_secs(),
                    "rsclaw: cached 308 redirect"
                );
            }

            hops += 1;
            if hops > MAX_REDIRECT_HOPS {
                anyhow::bail!("rsclaw: too many redirects ({MAX_REDIRECT_HOPS} hops) for {url}");
            }
            current_url = next_url;
        }
    }

    /// Convenience: a plain JSON POST that follows + caches redirects (no SSE,
    /// no idempotency, no per-hop timeout). For the simple fleet capability
    /// clients (OCR / embed / rerank).
    pub async fn post_json<B: Serialize>(
        &self,
        url: &str,
        body: &B,
        bearer: Option<&str>,
    ) -> Result<reqwest::Response> {
        self.post_following_redirects(url, body, bearer, false, None, None)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_extracts_scheme_host_port() {
        assert_eq!(origin_of("https://api.rsclaw.ai/v1/x"), Some("https://api.rsclaw.ai"));
        assert_eq!(
            origin_of("https://h:8443/p?q=1"),
            Some("https://h:8443")
        );
        assert_eq!(origin_of("not-a-url"), None);
    }

    #[test]
    fn rewrite_keeps_path() {
        assert_eq!(
            rewrite_origin("https://api.rsclaw.ai/v1/agent/vision", "https://d:8443"),
            "https://d:8443/v1/agent/vision"
        );
    }

    #[test]
    fn resolve_location_absolute_and_path() {
        assert_eq!(
            resolve_location("https://a.x/p", "https://b.y/q"),
            Some("https://b.y/q".to_owned())
        );
        assert_eq!(
            resolve_location("https://a.x/p", "/q"),
            Some("https://a.x/q".to_owned())
        );
        assert_eq!(resolve_location("https://a.x/p", "relative"), None);
        assert_eq!(resolve_location("https://a.x/p", ""), None);
    }

    #[test]
    fn max_age_parsing() {
        assert_eq!(parse_max_age(Some("max-age=3600")), Some(Duration::from_secs(3600)));
        assert_eq!(parse_max_age(Some("public, max-age=60")), Some(Duration::from_secs(60)));
        assert_eq!(parse_max_age(Some("no-store, max-age=60")), None);
        assert_eq!(parse_max_age(Some("no-cache")), None);
        assert_eq!(parse_max_age(None), None);
    }

    #[test]
    fn cache_store_lookup_invalidate() {
        let mut c = RedirectCache::default();
        c.store("https://a".into(), "https://b".into(), Duration::from_secs(60));
        assert_eq!(c.lookup("https://a"), Some("https://b".to_owned()));
        c.invalidate("https://a");
        assert_eq!(c.lookup("https://a"), None);
    }

    #[test]
    fn cache_evicts_expired() {
        let mut c = RedirectCache::default();
        c.store("https://a".into(), "https://b".into(), Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(c.lookup("https://a"), None);
    }
}
