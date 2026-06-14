//! URL identity canonicalization (Week 1: string only).
//!
//! Strips common tracking params, sorts the remaining query keys for
//! stability, lowercases scheme + host, and drops the fragment. The
//! result is suitable as a `logical_source_id` seed.
//!
//! No HTTP fetch happens here — `UrlCanonicalizer` (fetch + HTML→md)
//! lands in Week 2 alongside the embedder, worker pool, and
//! `UrlSyncer`. See plan goal + spec §I.

use anyhow::{Context, Result};

/// Canonicalize a URL into a form usable as `logical_source_id`.
pub fn canonicalize_url(raw: &str) -> Result<String> {
    let mut u = url::Url::parse(raw).context("parse url")?;
    let scheme = u.scheme().to_lowercase();
    let _ = u.set_scheme(&scheme);
    if let Some(host) = u.host_str() {
        let lc = host.to_lowercase();
        let _ = u.set_host(Some(&lc));
    }
    u.set_fragment(None);

    let pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !is_tracker(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut sorted = pairs;
    sorted.sort();
    u.query_pairs_mut().clear();
    for (k, v) in &sorted {
        u.query_pairs_mut().append_pair(k, v);
    }
    if u.query() == Some("") {
        u.set_query(None);
    }
    Ok(u.to_string())
}

fn is_tracker(k: &str) -> bool {
    if k.starts_with("utm_") {
        return true;
    }
    matches!(
        k,
        "fbclid"
            | "gclid"
            | "msclkid"
            | "yclid"
            | "dclid"
            | "_ga"
            | "_gl"
            | "ref"
            | "ref_src"
            | "ref_url"
            | "referrer"
            | "source"
            | "mc_cid"
            | "mc_eid"
            | "spm"       // taobao/aliexpress
            | "share_session_id"
            | "share_id"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_host_scheme() {
        assert_eq!(
            canonicalize_url("HTTPS://Example.COM/path").unwrap(),
            "https://example.com/path"
        );
    }

    #[test]
    fn strip_fragment() {
        assert_eq!(
            canonicalize_url("https://a.com/x#frag").unwrap(),
            "https://a.com/x"
        );
    }

    #[test]
    fn strip_utm() {
        let c = canonicalize_url("https://example.com/x?utm_source=a&utm_campaign=b&real=keep")
            .unwrap();
        assert!(!c.contains("utm_"));
        assert!(c.contains("real=keep"));
    }

    #[test]
    fn strip_common_trackers() {
        let c = canonicalize_url("https://x.com/p?fbclid=1&gclid=2&keep=yes").unwrap();
        assert!(!c.contains("fbclid"));
        assert!(!c.contains("gclid"));
        assert!(c.contains("keep=yes"));
    }

    #[test]
    fn sort_params_for_stability() {
        let a = canonicalize_url("https://x.com/p?b=2&a=1").unwrap();
        let b = canonicalize_url("https://x.com/p?a=1&b=2").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn tracking_only_url_collapses_query() {
        let c = canonicalize_url("https://x.com/p?utm_source=a").unwrap();
        assert_eq!(c, "https://x.com/p");
    }
}
