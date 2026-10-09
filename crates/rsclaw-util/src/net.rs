//! SSRF-safe outbound HTTP helpers.
//!
//! Every fetch whose URL can be influenced by an LLM, a plugin, a remote
//! peer or an inbound webhook payload MUST go through [`safe_send`] (or at
//! least [`resolve_public`] + a client from [`pinned_client`]). The guard:
//!
//! - only allows `http` / `https`;
//! - resolves the host once and rejects loopback, private, link-local,
//!   CGNAT, unique-local, multicast, documentation and IPv4-mapped/NAT64
//!   forms of those addresses;
//! - pins the connection to the vetted addresses so a second DNS answer
//!   (DNS rebinding) cannot redirect the request;
//! - disables reqwest's automatic redirects and re-validates every hop.
//!
//! When a proxy is configured the proxy performs DNS itself and pinning is a
//! no-op; the pre-flight check still rejects literal/private targets.

use std::net::{IpAddr, SocketAddr};
use std::sync::RwLock;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

/// Maximum redirect hops followed by [`safe_send`].
pub const DEFAULT_MAX_REDIRECTS: usize = 5;

/// True only for globally-routable unicast addresses.
pub fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || o[0] == 0
                // CGNAT 100.64.0.0/10
                || (o[0] == 100 && (o[1] & 0xc0) == 0x40)
                // 192.0.0.0/24 IETF protocol assignments
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                // 198.18.0.0/15 benchmarking
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
                // 240.0.0.0/4 reserved
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(&IpAddr::V4(v4));
            }
            let s = v6.segments();
            // NAT64 64:ff9b::/96 embeds an IPv4 address in the low 32 bits.
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|x| *x == 0) {
                let v4 = std::net::Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                );
                return is_public_ip(&IpAddr::V4(v4));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // IPv4-compatible ::/96 (deprecated) and ::ffff handled above
                || s[..6].iter().all(|x| *x == 0)
                // unique-local fc00::/7
                || (s[0] & 0xfe00) == 0xfc00
                // link-local fe80::/10
                || (s[0] & 0xffc0) == 0xfe80
                // site-local fec0::/10 (deprecated)
                || (s[0] & 0xffc0) == 0xfec0
                // documentation 2001:db8::/32
                || (s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}

/// Parse `raw`, require http(s), resolve the host and reject any non-public
/// target. Returns the parsed URL and the vetted socket addresses.
pub async fn resolve_public(raw: &str) -> Result<(url::Url, Vec<SocketAddr>)> {
    let parsed = url::Url::parse(raw).map_err(|e| anyhow!("invalid url: {e}"))?;
    let addrs = resolve_public_url(&parsed).await?;
    Ok((parsed, addrs))
}

/// Same as [`resolve_public`] for an already-parsed URL.
pub async fn resolve_public_url(parsed: &url::Url) -> Result<Vec<SocketAddr>> {
    match parsed.scheme() {
        "http" | "https" => {}
        other => bail!("url scheme `{other}` is not allowed (http/https only)"),
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("url has no host"))?;
    let host_l = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if host_l == "localhost" || host_l.ends_with(".localhost") || host_l.ends_with(".local") {
        bail!("url host `{host}` is not allowed (local address)");
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs: Vec<SocketAddr> = if let Ok(ip) = host_l.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::lookup_host((host_l.as_str(), port)),
        )
        .await
        .map_err(|_| anyhow!("dns lookup for `{host}` timed out"))?
        .map_err(|e| anyhow!("dns lookup for `{host}` failed: {e}"))?
        .collect()
    };
    if addrs.is_empty() {
        bail!("url host `{host}` did not resolve");
    }
    if let Some(bad) = addrs.iter().find(|a| !is_public_ip(&a.ip())) {
        bail!("url host `{host}` resolves to non-public address {}", bad.ip());
    }
    Ok(addrs)
}

/// Build a client for exactly one request to `url`, pinned to `addrs`, with
/// automatic redirects disabled. `base` carries caller settings (proxy, UA,
/// TLS); pass `reqwest::Client::builder()` when there are none.
pub fn pinned_client(
    base: reqwest::ClientBuilder,
    url: &url::Url,
    addrs: &[SocketAddr],
    timeout: Duration,
) -> Result<reqwest::Client> {
    let mut b = base
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout);
    if let Some(host) = url.host_str()
        && host.parse::<IpAddr>().is_err()
    {
        b = b.resolve_to_addrs(host, addrs);
    }
    b.build().map_err(|e| anyhow!("http client build failed: {e}"))
}

/// A request description for [`safe_send`].
#[derive(Debug, Clone)]
pub struct SafeRequest {
    /// HTTP method.
    pub method: reqwest::Method,
    /// Target URL (validated on every hop).
    pub url: String,
    /// Extra request headers. `Authorization` / `Cookie` are dropped when a
    /// redirect crosses origins.
    pub headers: reqwest::header::HeaderMap,
    /// Optional request body.
    pub body: Option<Vec<u8>>,
    /// Whole-request timeout per hop.
    pub timeout: Duration,
    /// Maximum redirect hops to follow.
    pub max_redirects: usize,
}

impl SafeRequest {
    /// Plain GET with default timeout (30s) and redirect limit.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: reqwest::Method::GET,
            url: url.into(),
            headers: reqwest::header::HeaderMap::new(),
            body: None,
            timeout: Duration::from_secs(30),
            max_redirects: DEFAULT_MAX_REDIRECTS,
        }
    }
}

/// Send `req` with SSRF protection on the initial URL and every redirect hop.
/// `base` is invoked once per hop to obtain a fresh builder carrying caller
/// settings (proxy, user agent, TLS roots).
pub async fn safe_send(
    base: impl Fn() -> reqwest::ClientBuilder,
    req: SafeRequest,
) -> Result<reqwest::Response> {
    let SafeRequest {
        mut method,
        url,
        mut headers,
        mut body,
        timeout,
        max_redirects,
    } = req;
    let mut current = url::Url::parse(&url).map_err(|e| anyhow!("invalid url: {e}"))?;
    for hop in 0..=max_redirects {
        let addrs = resolve_public_url(&current).await?;
        let client = pinned_client(base(), &current, &addrs, timeout)?;
        let mut rb = client
            .request(method.clone(), current.clone())
            .headers(headers.clone());
        if let Some(b) = &body {
            rb = rb.body(b.clone());
        }
        let resp = rb.send().await?;
        let status = resp.status();
        // Only follow real redirects; 300/304/305/306 are returned as-is so
        // callers can use conditional requests (If-None-Match etc).
        if !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok(resp);
        }
        if hop == max_redirects {
            bail!("too many redirects (>{max_redirects})");
        }
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("redirect {status} without Location"))?;
        let next = current
            .join(loc)
            .map_err(|e| anyhow!("bad redirect location `{loc}`: {e}"))?;
        if next.origin() != current.origin() {
            headers.remove(reqwest::header::AUTHORIZATION);
            headers.remove(reqwest::header::COOKIE);
            headers.remove(reqwest::header::PROXY_AUTHORIZATION);
        }
        if current.scheme() == "https" && next.scheme() == "http" {
            bail!("refusing https -> http redirect downgrade");
        }
        let code = status.as_u16();
        if code == 303 || ((code == 301 || code == 302) && method == reqwest::Method::POST) {
            method = reqwest::Method::GET;
            body = None;
            headers.remove(reqwest::header::CONTENT_TYPE);
            headers.remove(reqwest::header::CONTENT_LENGTH);
        }
        current = next;
    }
    bail!("too many redirects")
}

/// Read a response body, failing once more than `max_bytes` arrive.
pub async fn read_body_limited(mut resp: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    if let Some(len) = resp.content_length()
        && len as usize > max_bytes
    {
        bail!("response too large: {len} bytes (limit {max_bytes})");
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len() + chunk.len() > max_bytes {
            bail!("response exceeded {max_bytes} bytes");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Read a response body, silently truncating after `max_bytes`. Returns the
/// bytes and whether truncation happened.
pub async fn read_body_truncated(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = max_bytes.saturating_sub(out.len());
        if chunk.len() > room {
            out.extend_from_slice(&chunk[..room]);
            return Ok((out, true));
        }
        out.extend_from_slice(&chunk);
    }
    Ok((out, false))
}

/// Registrable domains trusted as one model-serving fleet when no
/// `gateway.trustedRedirectSites` is configured: the public API domain
/// permanently redirects to the serving domain.
pub const DEFAULT_TRUSTED_REDIRECT_SITES: &[&str] = &["rsclaw.ai", "duoduoyun.work"];

static TRUSTED_REDIRECT_SITES: RwLock<Option<Vec<String>>> = RwLock::new(None);

/// Replace the trusted redirect sites (`gateway.trustedRedirectSites`).
/// `None` restores [`DEFAULT_TRUSTED_REDIRECT_SITES`]; an explicit list,
/// even an empty one, replaces the defaults. Entries are registrable
/// domains (`example.com`); a leading `*.` or `.` is ignored.
pub fn set_trusted_redirect_sites(sites: Option<Vec<String>>) {
    let normalized = sites.map(|list| {
        list.iter()
            .map(|s| {
                s.trim()
                    .trim_start_matches("*.")
                    .trim_start_matches('.')
                    .to_ascii_lowercase()
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    });
    if let Ok(mut slot) = TRUSTED_REDIRECT_SITES.write() {
        *slot = normalized;
    }
}

/// Whether a credentialed https redirect between two registrable domains
/// stays inside the trusted fleet (both ends trusted).
pub fn is_trusted_redirect_hop(from_site: &str, to_site: &str) -> bool {
    let trusted = |site: &str, list: &[String]| list.iter().any(|t| t == site);
    let (a, b) = (from_site.to_ascii_lowercase(), to_site.to_ascii_lowercase());
    match TRUSTED_REDIRECT_SITES.read().ok().and_then(|g| g.clone()) {
        Some(list) => trusted(&a, &list) && trusted(&b, &list),
        None => {
            DEFAULT_TRUSTED_REDIRECT_SITES.contains(&a.as_str())
                && DEFAULT_TRUSTED_REDIRECT_SITES.contains(&b.as_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_mapped_addresses_are_rejected() {
        for s in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "fd00::1",
            "fe80::1",
            "::",
        ] {
            let ip: IpAddr = s.parse().expect("ip");
            assert!(!is_public_ip(&ip), "{s} should be rejected");
        }
        for s in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            let ip: IpAddr = s.parse().expect("ip");
            assert!(is_public_ip(&ip), "{s} should be allowed");
        }
    }

    #[tokio::test]
    async fn literal_private_urls_are_rejected() {
        for u in [
            "http://127.0.0.1:18888/api/v1/shutdown",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost/",
            "file:///etc/passwd",
            "http://169.254.169.254/latest/meta-data",
        ] {
            assert!(resolve_public(u).await.is_err(), "{u} should be rejected");
        }
    }

    #[test]
    fn trusted_redirect_sites_default_and_override() {
        set_trusted_redirect_sites(None);
        assert!(is_trusted_redirect_hop("rsclaw.ai", "duoduoyun.work"));
        assert!(!is_trusted_redirect_hop("rsclaw.ai", "evil.com"));

        set_trusted_redirect_sites(Some(vec![" *.Example.com ".into(), ".cdn.net".into()]));
        assert!(is_trusted_redirect_hop("example.com", "cdn.net"));
        assert!(!is_trusted_redirect_hop("rsclaw.ai", "duoduoyun.work"));

        set_trusted_redirect_sites(Some(vec![]));
        assert!(!is_trusted_redirect_hop("rsclaw.ai", "duoduoyun.work"));

        set_trusted_redirect_sites(None);
        assert!(is_trusted_redirect_hop("duoduoyun.work", "rsclaw.ai"));
    }
}
