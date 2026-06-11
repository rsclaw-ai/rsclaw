//! Typed HTTP client for the astock `/v1/*` endpoints.
//!
//! Surface and field shapes are tracked against `oopos/astock` —
//! specifically `gateway/internal/tdx/client.go` (Quote/Kline/Level)
//! and `gateway/internal/api/handlers.go` (request/response wrapping).
//! Reproducing the shapes verbatim (not `serde_json::Value` blobs)
//! gives the agent runtime / CLI proper Rust types AND a single
//! source of truth for the API contract — if astock renames a field,
//! the build breaks instead of the runtime silently returning empty
//! values.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::config::schema::AstockConfig;

/// Conservative defaults applied when the user didn't specify a
/// cache TTL. Quotes change every tick during trading hours but
/// LLMs ask the same quote 3-5 times per turn while reasoning; 5s
/// is the sweet spot.
const DEFAULT_QUOTE_TTL_SECS: u64 = 5;
const DEFAULT_KLINE_TTL_SECS: u64 = 30;
const DEFAULT_SNAPSHOT_TTL_SECS: u64 = 60;

/// HTTP request timeout. Quote / kline come back in <500ms typically;
/// `/v1/ask` calls the iwencai upstream and can take 2-5s. 15s is the
/// "real failure" threshold above which we'd rather error than block
/// the IM turn.
const HTTP_TIMEOUT_SECS: u64 = 15;

/// Errors surfaced from the astock client. Distinguishes the cases
/// callers actually want to branch on (not configured vs upstream
/// down vs auth failed vs schema mismatch) so the LLM tool layer can
/// give targeted error messages instead of a generic "internal
/// error".
#[derive(Debug, thiserror::Error)]
pub enum AstockError {
    /// `config.astock.enabled` is false or the config block is
    /// missing entirely. Returned by `from_config` (not at runtime).
    #[error("astock subsystem is not configured (set config.astock.enabled + baseUrl)")]
    NotConfigured,
    /// astock gateway returned a non-2xx status. Body slice (first
    /// 400 chars) included for diagnostics.
    #[error("astock returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    /// Transport failure — DNS, connection refused, TLS, timeout.
    #[error("astock transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// astock returned 2xx but the JSON didn't deserialize. Implies
    /// a schema drift; logged loudly so we notice instead of
    /// silently dropping data.
    #[error("astock JSON shape mismatch: {0}")]
    Decode(#[from] serde_json::Error),
}

/// One real-time quote row, matching `tdx.Quote` in astock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub code: String,
    #[serde(default)]
    pub market: String,
    pub price: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub pre_close: f64,
    pub volume: i64,
    pub amount: f64,
    #[serde(default)]
    pub bids: Vec<QuoteLevel>,
    #[serde(default)]
    pub asks: Vec<QuoteLevel>,
    pub timestamp: i64,
}

impl Quote {
    /// Convenience: percent change vs prior close. Returns 0.0 when
    /// pre_close is missing/zero to avoid divide-by-zero panics in
    /// formatters. Same definition used downstream by Chinese
    /// brokers (涨跌幅).
    pub fn change_pct(&self) -> f64 {
        if self.pre_close.abs() < f64::EPSILON {
            0.0
        } else {
            (self.price - self.pre_close) / self.pre_close * 100.0
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteLevel {
    pub price: f64,
    pub volume: i64,
}

/// One K-line bar, matching `tdx.Kline`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KlineBar {
    pub timestamp: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub amount: f64,
}

/// Wrap for `/v1/kline/:code` response (the handler wraps the bars
/// with a few metadata fields the raw `tdx.Kline` doesn't carry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KlineResp {
    pub code: String,
    pub period: String,
    pub klines: Vec<KlineBar>,
    #[serde(default)]
    pub adjust: String,
    /// Set only when astock degraded the adjust factor lookup to
    /// `none` mid-request. Carry it through so the agent can flag
    /// "raw prices, no adjustment" instead of silently lying about
    /// what was returned.
    #[serde(default)]
    pub adjust_warning: Option<String>,
    /// Coverage hint by year (high / medium / experimental).
    /// astock uses this to flag pre-2010 data as archival-quality.
    #[serde(default)]
    pub data_quality: serde_json::Value,
}

/// Snapshot row — fields not modeled here are tolerated by the
/// untagged `serde_json::Value` fallback in `extra`, so an upstream
/// addition (e.g. a new turnover metric) doesn't break the build
/// even though we won't surface it without a typed accessor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRow {
    pub code: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub market: String,
    #[serde(default)]
    pub price: f64,
    #[serde(default)]
    pub pre_close: f64,
    #[serde(default)]
    pub volume: i64,
    #[serde(default)]
    pub amount: f64,
    #[serde(default)]
    pub timestamp: i64,
    /// Catch-all for fields not modeled above — keeps forward
    /// compatibility with astock additions.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Raw iwencai response from `/v1/ask`. astock passes through what
/// iwencai returns, which is structurally `{ headers, data, ... }`
/// but the field set varies by query class — so we keep it as a
/// `Value` and let the formatter pick out what it needs.
pub type AskResp = serde_json::Value;

/// Generic SQL query response (`/v1/query` returns `{ columns, rows
/// }`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResp {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
}

/// In-memory TTL cache keyed by string. Single mutex around a tiny
/// HashMap — endpoints are low-cardinality (a few dozen watched
/// codes per user) so contention is a non-issue. Replace with an
/// LRU only when we start seeing the map grow past a few hundred
/// entries during a long session.
struct TtlCache<V: Clone> {
    inner: Mutex<HashMap<String, (Instant, V)>>,
    ttl: Duration,
}

impl<V: Clone> TtlCache<V> {
    fn new(ttl_secs: u64) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    fn get(&self, key: &str) -> Option<V> {
        // Zero-TTL cache acts as a no-op: every lookup misses, every
        // insert is silently dropped on the next read.
        if self.ttl.is_zero() {
            return None;
        }
        let g = self.inner.lock().ok()?;
        let (ts, v) = g.get(key)?;
        if ts.elapsed() > self.ttl {
            return None;
        }
        Some(v.clone())
    }

    fn put(&self, key: String, value: V) {
        if self.ttl.is_zero() {
            return;
        }
        if let Ok(mut g) = self.inner.lock() {
            g.insert(key, (Instant::now(), value));
        }
    }
}

/// Typed astock HTTP client. Hold one of these `Arc`-wrapped for
/// the gateway lifetime; the inner `reqwest::Client` reuses
/// connections per the upstream service, and the caches deduplicate
/// the LLM-tool re-asks-same-quote pattern.
pub struct AstockClient {
    http: reqwest::Client,
    base_url: String,
    auth_token: Option<String>,
    quote_cache: TtlCache<Quote>,
    kline_cache: TtlCache<KlineResp>,
    snapshot_cache: TtlCache<Vec<SnapshotRow>>,
}

// Manual `Debug` impl — `TtlCache` and the caches behind it carry
// `Mutex<HashMap<..>>` which doesn't auto-derive on stable, and we
// don't want to spam `#[derive(Debug)]` on the cache type and
// surrounding internals. Redact the auth token defensively.
impl std::fmt::Debug for AstockClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AstockClient")
            .field("base_url", &self.base_url)
            .field("auth_token", &self.auth_token.as_deref().map(|_| "<redacted>"))
            .finish()
    }
}

impl AstockClient {
    /// Build a client from the parsed config. Returns
    /// `NotConfigured` when the block is missing or `enabled =
    /// false`; in that case the gateway leaves the global slot empty
    /// so `global_client()` returns `None` and downstream tools
    /// skip cleanly.
    pub fn from_config(cfg: Option<&AstockConfig>) -> Result<Self, AstockError> {
        let cfg = cfg.ok_or(AstockError::NotConfigured)?;
        if !cfg.enabled.unwrap_or(false) {
            return Err(AstockError::NotConfigured);
        }
        let base = cfg
            .base_url
            .as_deref()
            .map(|s| s.trim().trim_end_matches('/').to_owned())
            .filter(|s| !s.is_empty())
            .ok_or(AstockError::NotConfigured)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(3))
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .pool_max_idle_per_host(8)
            .build()?;
        let cache = cfg.cache.as_ref();
        let quote_ttl = cache.and_then(|c| c.quote_secs).map(u64::from).unwrap_or(DEFAULT_QUOTE_TTL_SECS);
        let kline_ttl = cache.and_then(|c| c.kline_secs).map(u64::from).unwrap_or(DEFAULT_KLINE_TTL_SECS);
        let snapshot_ttl = cache
            .and_then(|c| c.snapshot_secs)
            .map(u64::from)
            .unwrap_or(DEFAULT_SNAPSHOT_TTL_SECS);
        Ok(Self {
            http,
            base_url: base,
            auth_token: cfg
                .auth_token
                .as_ref()
                .and_then(|s| s.resolve_early())
                .filter(|s| !s.trim().is_empty()),
            quote_cache: TtlCache::new(quote_ttl),
            kline_cache: TtlCache::new(kline_ttl),
            snapshot_cache: TtlCache::new(snapshot_ttl),
        })
    }

    /// Resolve a batch of stock codes to display names.
    ///
    /// Currently a no-op: as of 2026-06-11 the upstream astock service
    /// does NOT carry stock-name data on ANY endpoint or table —
    /// verified by:
    ///   * `GET /v1/quote/600519`     → no `name` field
    ///   * `GET /v1/snapshot?codes=…` → no `name` field (despite our
    ///     `SnapshotRow.name: Option<String>` being declared, the
    ///     server never populates it)
    ///   * `SELECT column_name FROM information_schema.columns ...` →
    ///     no name column anywhere in the duckdb store
    ///   * `POST /v1/ask` (iwencai) → would work but needs an
    ///     `IWENCAI_API_KEY` we don't currently have
    ///
    /// The infrastructure here (cache + signature) is kept so callers
    /// can opt in without changes once a real source lands —
    /// upstream is the natural place (free Chinese stock data
    /// providers like sina / eastmoney expose names trivially), or we
    /// bundle a static `code → name` JSON at build time (~150KB for
    /// top 5000 A-shares).
    ///
    /// Until then, ALWAYS returns an empty map → callers render
    /// code-only. The cache is still maintained as "we asked, got
    /// nothing" sentinel so future opt-ins don't re-ask hot codes.
    pub async fn resolve_names(
        &self,
        _codes: &[String],
    ) -> std::collections::HashMap<String, String> {
        // No-op for v1. See doc comment for rationale.
        std::collections::HashMap::new()
    }

    /// astock `GET /v1/screen/quick/<filter>?limit=<n>`. One-shot
    /// snapshot of a quick screener — same data set the SSE bridge
    /// streams continuously, but returned in full at the moment of
    /// call. Returns the raw JSON tree (the response shape carries
    /// `results: [...]`, `count`, `cost_credits`, etc.) so the
    /// caller can format it however it likes.
    ///
    /// Not cached — screen results change frequently and the user
    /// asking via `/astock screen quick_rally` is specifically asking
    /// for a NOW snapshot.
    pub async fn screen_quick(
        &self,
        filter: &str,
        limit: Option<u32>,
    ) -> Result<serde_json::Value, AstockError> {
        let mut url = format!("{}/v1/screen/quick/{}", self.base_url, filter);
        if let Some(n) = limit {
            url.push_str(&format!("?limit={n}"));
        }
        self.get_json(&url).await
    }

    /// astock `GET /v1/quote/:code`. Cached per TTL (default 5s).
    pub async fn quote(&self, code: &str) -> Result<Quote, AstockError> {
        let code = normalize_code(code);
        if let Some(hit) = self.quote_cache.get(&code) {
            return Ok(hit);
        }
        let url = format!("{}/v1/quote/{}", self.base_url, code);
        let q: Quote = self.get_json(&url).await?;
        self.quote_cache.put(code, q.clone());
        Ok(q)
    }

    /// astock `POST /v1/quote/batch`. NOT cached — batch composition
    /// is per-call, caching at the batch level would rarely hit.
    /// Callers that want per-code caching should issue N `quote()`
    /// calls in parallel.
    pub async fn quote_batch(&self, codes: &[String]) -> Result<Vec<Quote>, AstockError> {
        let codes: Vec<String> = codes.iter().map(|c| normalize_code(c)).collect();
        let body = serde_json::json!({ "codes": codes });
        let url = format!("{}/v1/quote/batch", self.base_url);
        let wrap: serde_json::Value = self.post_json(&url, &body).await?;
        let quotes = wrap
            .get("quotes")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        Ok(serde_json::from_value(quotes)?)
    }

    /// astock `GET /v1/kline/:code?period=&count=&offset=&adjust=`.
    /// Defaults match the upstream handler: 200 bars, no offset, raw
    /// (no factor adjustment).
    pub async fn kline(
        &self,
        code: &str,
        period: Option<&str>,
        count: Option<u32>,
        offset: Option<u32>,
        adjust: Option<&str>,
    ) -> Result<KlineResp, AstockError> {
        let code = normalize_code(code);
        let period = period.unwrap_or("1d");
        let count = count.unwrap_or(200).clamp(1, 800);
        let offset = offset.unwrap_or(0);
        let adjust = adjust.unwrap_or("none");
        let cache_key = format!("{code}|{period}|{count}|{offset}|{adjust}");
        if let Some(hit) = self.kline_cache.get(&cache_key) {
            return Ok(hit);
        }
        let url = format!(
            "{}/v1/kline/{}?period={}&count={}&offset={}&adjust={}",
            self.base_url, code, period, count, offset, adjust
        );
        let r: KlineResp = self.get_json(&url).await?;
        self.kline_cache.put(cache_key, r.clone());
        Ok(r)
    }

    /// astock `GET /v1/snapshot`. Live or historical (when `ts` is
    /// set). Filters by market or explicit code list. Cached only for
    /// the LIVE no-filter all-market form (the heavy one); filtered
    /// calls go through every time since they're cheap.
    pub async fn snapshot(
        &self,
        ts: Option<&str>,
        market: Option<&str>,
        codes: &[String],
        adjust: Option<&str>,
    ) -> Result<Vec<SnapshotRow>, AstockError> {
        let cache_eligible = ts.is_none() && market.is_none() && codes.is_empty();
        let cache_key = "live:all".to_owned();
        if cache_eligible {
            if let Some(hit) = self.snapshot_cache.get(&cache_key) {
                return Ok(hit);
            }
        }
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(v) = ts {
            params.push(("ts", v.to_owned()));
        }
        if let Some(v) = market {
            params.push(("market", v.to_owned()));
        }
        if !codes.is_empty() {
            let joined: Vec<String> = codes.iter().map(|c| normalize_code(c)).collect();
            params.push(("codes", joined.join(",")));
        }
        if let Some(v) = adjust {
            params.push(("adjust", v.to_owned()));
        }
        let url = format!("{}/v1/snapshot", self.base_url);
        let rb = self.http.get(&url).query(&params);
        let rb = self.with_auth(rb);
        let resp = rb.send().await?;
        // Astock wraps snapshot rows in
        //   { "count": N, "snapshots": [...], "market": "...", "ts": ... }
        // The bare `Vec<SnapshotRow>` decode shape used to be the upstream
        // contract — at some point astock added the wrapper and we kept
        // decoding into a bare array, which silently failed serde
        // deserialisation. Side effect: `tool_stock_snapshot` returned an
        // error for every non-trivial call, and `resolve_names` got an
        // empty map every time (so `/astock quote` never showed Chinese
        // names). Unwrap the `snapshots` key defensively — fall back to
        // treating the body as a bare array for back-compat in case the
        // shape regresses.
        let body: serde_json::Value = self.read_json(resp).await?;
        let rows: Vec<SnapshotRow> = match body.get("snapshots") {
            Some(arr) => serde_json::from_value(arr.clone())?,
            None => serde_json::from_value(body)?,
        };
        if cache_eligible {
            self.snapshot_cache.put(cache_key, rows.clone());
        }
        Ok(rows)
    }

    /// astock `POST /v1/ask`. Natural-language query proxied through
    /// iwencai (同花顺). This is the killer endpoint: hand the LLM's
    /// human-phrased question straight to astock and let iwencai
    /// turn it into a structured result, instead of asking the LLM
    /// to assemble screen filters itself.
    pub async fn ask(
        &self,
        query: &str,
        page: Option<u32>,
        limit: Option<u32>,
        call_type: Option<&str>,
    ) -> Result<AskResp, AstockError> {
        let mut body = serde_json::json!({ "query": query });
        if let Some(p) = page {
            body["page"] = serde_json::json!(p);
        }
        if let Some(l) = limit {
            body["limit"] = serde_json::json!(l);
        }
        if let Some(c) = call_type {
            body["call_type"] = serde_json::json!(c);
        }
        let url = format!("{}/v1/ask", self.base_url);
        self.post_json(&url, &body).await
    }

    /// astock `POST /v1/query`. Read-only SQL over the DuckDB store —
    /// astock validates safety server-side; we pass through.
    pub async fn query_sql(&self, sql: &str) -> Result<QueryResp, AstockError> {
        let body = serde_json::json!({ "sql": sql });
        let url = format!("{}/v1/query", self.base_url);
        self.post_json(&url, &body).await
    }

    // -----------------------------------------------------------------
    // HTTP plumbing
    // -----------------------------------------------------------------

    fn with_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth_token {
            Some(t) => rb.header("Authorization", format!("Bearer {t}")),
            None => rb,
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, AstockError> {
        let resp = self.with_auth(self.http.get(url)).send().await?;
        self.read_json(resp).await
    }

    async fn post_json<B: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, AstockError> {
        let resp = self.with_auth(self.http.post(url).json(body)).send().await?;
        self.read_json(resp).await
    }

    async fn read_json<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, AstockError> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(400)
                .collect::<String>();
            return Err(AstockError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Normalize a user-supplied stock code to the canonical 6-digit
/// form astock expects. Strips common decorations (`SH:600519`,
/// `600519.SH`, `sh600519`, `600519 `, fullwidth digits left as-is
/// because they're not valid anyway). Bare 6-digit codes pass
/// through unchanged.
///
/// We deliberately do NOT add a market suffix here — astock infers
/// the market from the first digit (6→SH, 0/3→SZ, 8/9→BJ) and our
/// adding `.SH` would prepend a redundant token in the URL path.
fn normalize_code(s: &str) -> String {
    let s = s.trim();
    let s = s.trim_start_matches("SH:").trim_start_matches("SZ:").trim_start_matches("BJ:");
    let s = s.trim_start_matches("sh:").trim_start_matches("sz:").trim_start_matches("bj:");
    let s = s.trim_start_matches("SH").trim_start_matches("SZ").trim_start_matches("BJ");
    let s = s.trim_start_matches("sh").trim_start_matches("sz").trim_start_matches("bj");
    // Drop .SH / .SZ / .BJ suffix if present.
    let s = s
        .split_once('.')
        .map(|(head, _)| head)
        .unwrap_or(s);
    let s = s.trim().to_owned();
    // Validate: must be non-empty and exactly 6 digits for A-share codes.
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_digit()) {
        // Return the original trimmed input so astock can produce a
        // meaningful 404 instead of silently failing with an empty path.
        return s.trim().to_owned();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_prefixes_and_suffixes() {
        assert_eq!(normalize_code("600519"), "600519");
        assert_eq!(normalize_code(" 600519 "), "600519");
        assert_eq!(normalize_code("sh600519"), "600519");
        assert_eq!(normalize_code("SH600519"), "600519");
        assert_eq!(normalize_code("SH:600519"), "600519");
        assert_eq!(normalize_code("600519.SH"), "600519");
        assert_eq!(normalize_code("000001.SZ"), "000001");
        assert_eq!(normalize_code("300750.SZ"), "300750");
        assert_eq!(normalize_code("832566.BJ"), "832566");
    }

    #[test]
    fn change_pct_handles_zero_preclose() {
        let q = Quote {
            code: "X".into(),
            market: String::new(),
            price: 10.0,
            open: 10.0,
            high: 10.0,
            low: 10.0,
            pre_close: 0.0,
            volume: 0,
            amount: 0.0,
            bids: vec![],
            asks: vec![],
            timestamp: 0,
        };
        assert_eq!(q.change_pct(), 0.0, "pre_close==0 must not divide-by-zero");
    }

    #[test]
    fn change_pct_typical() {
        let q = Quote {
            code: "X".into(),
            market: String::new(),
            price: 105.0,
            open: 100.0,
            high: 106.0,
            low: 99.0,
            pre_close: 100.0,
            volume: 0,
            amount: 0.0,
            bids: vec![],
            asks: vec![],
            timestamp: 0,
        };
        assert!((q.change_pct() - 5.0).abs() < 1e-6);
    }

    #[test]
    fn ttl_cache_misses_when_zero() {
        let c = TtlCache::<u32>::new(0);
        c.put("k".into(), 1);
        assert!(c.get("k").is_none(), "ttl=0 must disable cache reads");
    }

    #[test]
    fn ttl_cache_hits_within_ttl() {
        let c = TtlCache::<u32>::new(60);
        c.put("k".into(), 7);
        assert_eq!(c.get("k"), Some(7));
    }

    #[test]
    fn from_config_returns_not_configured_when_disabled() {
        let cfg = AstockConfig {
            enabled: Some(false),
            base_url: Some("http://127.0.0.1:19999".into()),
            ..Default::default()
        };
        let err = AstockClient::from_config(Some(&cfg)).unwrap_err();
        assert!(matches!(err, AstockError::NotConfigured));
    }

    #[test]
    fn from_config_returns_not_configured_when_missing() {
        let err = AstockClient::from_config(None).unwrap_err();
        assert!(matches!(err, AstockError::NotConfigured));
    }

    #[test]
    fn from_config_requires_base_url() {
        let cfg = AstockConfig {
            enabled: Some(true),
            base_url: None,
            ..Default::default()
        };
        let err = AstockClient::from_config(Some(&cfg)).unwrap_err();
        assert!(matches!(err, AstockError::NotConfigured));
    }

    #[test]
    fn from_config_ok_with_minimal_inputs() {
        let cfg = AstockConfig {
            enabled: Some(true),
            base_url: Some("http://127.0.0.1:19999".into()),
            ..Default::default()
        };
        let c = AstockClient::from_config(Some(&cfg)).expect("should build");
        assert_eq!(c.base_url, "http://127.0.0.1:19999");
        assert!(c.auth_token.is_none());
    }
}

/// Compile-time silencer for `anyhow::Result` so this module's
/// `pub use` from `mod.rs` doesn't flag an unused import; we surface
/// `AstockError` directly but keep `Result` available for callers
/// that want to wrap it.
#[allow(dead_code)]
fn _force_use_anyhow() -> Result<()> {
    Err(anyhow!("not called"))
}
