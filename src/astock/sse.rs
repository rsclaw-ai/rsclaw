//! astock SSE → IM push bridge.
//!
//! Subscribes to astock's `GET /v1/stream/quick?filter=<name>` SSE
//! endpoints (one listener task per configured filter). Each `hit`
//! event carries `{filter, code, name?, diag, ts}`. We cross-
//! reference the code against every peer's watchlist and, on hit,
//! push a pre-formatted markdown notification directly to the IM
//! channel — no LLM turn, no task queue, sub-second latency.
//!
//! Connection lifecycle (matches astock's protocol):
//!
//! 1. `GET /v1/stream/quick?filter=<f>` with `Last-Event-ID` header
//!    on reconnects so we resume the ring instead of double-firing
//!    notifications across a network blip.
//! 2. Parse event stream: lines `id: <int>`, `event: <name>`,
//!    `data: <json>`, blank-line dispatch.
//! 3. Per event type:
//!    - `hit` → look up peers + push notification (this is the
//!      load-bearing path).
//!    - `drop` → ignored in v1; we don't notify on transitions
//!      OUT of the filter set because the user already knows the
//!      stock is no longer "rallying".
//!    - `snapshot` → received once per connection (initial /
//!      after stale_reset). We DON'T push these as notifications —
//!      a fresh subscriber would otherwise get a wall of "X is
//!      currently rallying" messages every reconnect. Logged only.
//!    - `stale_reset` → reset `last_event_id` to 0 so the next
//!      `Last-Event-ID` header doesn't try to resume from an ID
//!      that's no longer in the ring.
//! 4. Exponential backoff on disconnect: 1s → 2s → ... → 60s cap,
//!    reset to 1s after a successful flag (first event received).
//!
//! Per-peer-per-code dedupe: the same code can hit multiple
//! adjacent ticks if its diagnostic flips just over the threshold.
//! Each listener keeps a small LRU of recently-pushed
//! `(peer, code, ts/60)` keys so we don't fire two notifications
//! within the same minute for the same code on the same peer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::agent::memory::MemoryStore;
use crate::channel::OutboundMessage;
use crate::config::schema::AstockConfig;

/// Reasonable defaults when `config.astock.sse` is missing entries.
const DEFAULT_FILTERS: &[&str] = &[
    "quick_rally",       // 异动: 拉升
    "quick_goldcross",   // 金叉
    "quick_cow_catch",   // 抓牛股
];

/// Per-peer-per-code dedupe window. 60s is long enough to suppress
/// a chatty filter that re-fires on the same code multiple times
/// per minute, short enough that a genuine second-day signal still
/// reaches the user.
const DEDUPE_WINDOW: Duration = Duration::from_secs(60);

/// Cap how many dedupe entries we keep per listener. Each entry is
/// (peer_id, code, bucket) so 1024 covers ~1000 unique signals
/// in-flight — more than the SSE ring's capacity anyway.
const DEDUPE_LRU_CAP: usize = 1024;

/// Spawn N long-lived SSE listener tasks (one per configured
/// filter). Gated on the astock client being initialised — caller
/// should not invoke this when astock is dormant. Idempotent at the
/// per-filter level: spawning twice means two listeners on the
/// same filter, which would double-push notifications, so the
/// gateway startup MUST call this only once.
pub fn spawn_listeners(cfg: &AstockConfig) {
    let Some(base_url) = cfg.base_url.as_deref().filter(|s| !s.is_empty()) else {
        tracing::info!("astock SSE: no baseUrl; skipping listener spawn");
        return;
    };
    let sse_cfg = cfg.sse.as_ref();
    let sse_enabled = sse_cfg.and_then(|s| s.enabled).unwrap_or(true);
    if !sse_enabled {
        tracing::info!("astock SSE: disabled via config; skipping listener spawn");
        return;
    }
    let filters: Vec<String> = sse_cfg
        .and_then(|s| s.filters.clone())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_FILTERS.iter().map(|s| (*s).to_owned()).collect());
    let token = cfg.auth_token.as_ref().and_then(|s| s.resolve_early());
    let base_url = base_url.trim_end_matches('/').to_owned();

    for filter in filters {
        let base = base_url.clone();
        let tok = token.clone();
        let f = filter.clone();
        tokio::spawn(async move {
            run_listener(base, tok, f).await;
        });
        tracing::info!(filter, "astock SSE listener spawned");
    }
}

/// Persistent connect-with-backoff loop. Never returns under normal
/// operation; the task lives until the gateway exits.
async fn run_listener(base_url: String, auth_token: Option<String>, filter: String) {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        // No total-request timeout — SSE is a long-lived stream and a
        // bound here would tear it down every <timeout> seconds.
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(filter, error = %e, "astock SSE: HTTP client build failed; listener exiting");
            return;
        }
    };
    let dedupe: Arc<Mutex<DedupeLru>> = Arc::new(Mutex::new(DedupeLru::new(DEDUPE_LRU_CAP)));
    let mut last_event_id: Option<i64> = None;
    let mut backoff = Duration::from_secs(1);
    let mut consecutive_errs: u32 = 0;
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    loop {
        match connect_and_stream(
            &client,
            &base_url,
            auth_token.as_deref(),
            &filter,
            &mut last_event_id,
            &dedupe,
        )
        .await
        {
            Ok(()) => {
                if consecutive_errs >= 5 {
                    tracing::info!(filter, after_failures = consecutive_errs, "astock SSE: recovered");
                }
                consecutive_errs = 0;
                tracing::info!(filter, "astock SSE: stream ended cleanly; reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                // Same escalation contract as wechat/feishu reconnect loops:
                // singles are debug weather, streaks are the outage signal.
                consecutive_errs = consecutive_errs.saturating_add(1);
                if consecutive_errs == 5 || (consecutive_errs > 5 && consecutive_errs % 10 == 0) {
                    tracing::warn!(
                        filter,
                        error = %e,
                        consecutive = consecutive_errs,
                        backoff_secs = backoff.as_secs(),
                        "astock SSE: failing repeatedly; reconnecting after backoff"
                    );
                } else {
                    tracing::debug!(
                        filter,
                        error = %e,
                        consecutive = consecutive_errs,
                        backoff_secs = backoff.as_secs(),
                        "astock SSE: listener error; reconnecting after backoff"
                    );
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One connection lifecycle: connect, parse SSE frames, dispatch
/// each `hit` to matching peers. Returns Ok on clean EOF, Err on
/// transport failure or non-2xx status.
async fn connect_and_stream(
    client: &reqwest::Client,
    base_url: &str,
    auth_token: Option<&str>,
    filter: &str,
    last_event_id: &mut Option<i64>,
    dedupe: &Arc<Mutex<DedupeLru>>,
) -> anyhow::Result<()> {
    let url = format!("{base_url}/v1/stream/quick?filter={filter}");
    let mut req = client.get(&url).header("Accept", "text/event-stream");
    if let Some(t) = auth_token.filter(|s| !s.is_empty()) {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    if let Some(id) = *last_event_id {
        req = req.header("Last-Event-ID", id.to_string());
    }
    let resp = req.send().await?.error_for_status()?;
    tracing::info!(
        filter,
        resume_from = ?last_event_id,
        "astock SSE: connected"
    );
    let mut got_first_event = false;
    let mut buf = String::new();
    let mut event_id: Option<i64> = None;
    let mut event_name: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();

    let mut byte_stream = resp.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        // Parse line-by-line; dispatch on blank-line delimiter.
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_owned();
            buf.drain(..=nl);
            if line.is_empty() {
                // End of one event frame.
                if let (Some(name), data) = (event_name.as_deref(), data_lines.join("\n")) {
                    if !data.is_empty() {
                        got_first_event = true;
                        if let Some(id) = event_id {
                            *last_event_id = Some(id);
                        }
                        dispatch_event(name, &data, filter, dedupe).await;
                    }
                }
                event_id = None;
                event_name = None;
                data_lines.clear();
                continue;
            }
            if let Some(rest) = line.strip_prefix("id: ") {
                event_id = rest.trim().parse().ok();
            } else if let Some(rest) = line.strip_prefix("event: ") {
                event_name = Some(rest.trim().to_owned());
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data_lines.push(rest.to_owned());
            } else if line.starts_with("retry: ") || line.starts_with(":") {
                // retry hint or comment / heartbeat — ignore
            }
        }
    }
    if !got_first_event {
        anyhow::bail!("astock SSE: stream closed before first event");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct HitEvent {
    #[serde(default)]
    filter: String,
    code: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    ts: i64,
    /// Filter-specific diagnostic — pass-through for the notification
    /// body. Untyped because every filter has its own diag shape.
    #[serde(default)]
    diag: serde_json::Value,
}

/// Per-event-type handler. Only `hit` and `stale_reset` are
/// actionable in v1; everything else is a no-op or log-only.
async fn dispatch_event(
    event_name: &str,
    data: &str,
    filter: &str,
    dedupe: &Arc<Mutex<DedupeLru>>,
) {
    match event_name {
        "hit" => {
            let evt: HitEvent = match serde_json::from_str(data) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(filter, error = %e, "astock SSE: failed to parse hit event");
                    return;
                }
            };
            handle_hit(evt, filter, dedupe).await;
        }
        "snapshot" => {
            // Initial subscriber state — DO NOT push. See module docs.
            tracing::debug!(filter, "astock SSE: snapshot received (initial state, no push)");
        }
        "stale_reset" => {
            tracing::info!(filter, "astock SSE: stale_reset; clearing dedupe");
            dedupe.lock().await.clear();
        }
        "drop" => {
            // Transition OUT of filter set — ignored in v1.
        }
        other => {
            tracing::debug!(filter, event = other, "astock SSE: ignoring unknown event");
        }
    }
}

/// Cross-reference the hit code with every peer's watchlist and
/// push notifications to matching peers. Uses the same scope shape
/// `agent:{id}:watchlist:{channel}:{peer}` that `stock_watchlist`
/// writes to.
async fn handle_hit(evt: HitEvent, filter: &str, dedupe: &Arc<Mutex<DedupeLru>>) {
    let Some(mem) = crate::agent::memory::global_store() else {
        return;
    };
    let groups = collect_peers_watching_code(&mem, &evt.code).await;
    if groups.is_empty() {
        tracing::debug!(filter, code = %evt.code, "astock SSE: no peer watches this code");
        return;
    }
    let display_name = evt.name.as_deref().unwrap_or("").trim();
    let summary = format_hit_summary(filter, &evt.code, display_name, &evt.diag, evt.ts);
    let now = Instant::now();
    let bucket = (evt.ts / 60).max(0);

    for (channel, account, peer_id) in groups {
        // Dedupe by (peer, code, minute_bucket). Prevents two pushes
        // from adjacent ticks within the same minute.
        let dedupe_key = format!("{peer_id}|{code}|{bucket}", code = evt.code);
        {
            let mut g = dedupe.lock().await;
            if g.contains(&dedupe_key, now) {
                continue;
            }
            g.insert(dedupe_key, now);
        }
        let msg = OutboundMessage {
            target_id: peer_id.clone(),
            is_group: false,
            text: summary.clone(),
            reply_to: None,
            images: vec![],
            files: vec![],
            channel: Some(channel.clone()),
            account: account.clone(),
        };
        if let Err(e) = crate::gateway::task_queue::push_outbound(
            &channel,
            account.as_deref(),
            msg,
        ) {
            tracing::warn!(
                channel = %channel,
                peer = %peer_id,
                code = %evt.code,
                error = %e,
                "astock SSE: failed to push notification"
            );
        } else {
            tracing::info!(
                filter,
                channel = %channel,
                peer = %peer_id,
                code = %evt.code,
                "astock SSE: notification pushed"
            );
        }
    }
}

/// Format the IM body for one hit. Plain text + `[code 名称] 触发
/// <filter>` + a one-line diag summary when available. Mirrors the
/// `stock_quote` markdown style.
fn format_hit_summary(
    filter: &str,
    code: &str,
    name: &str,
    diag: &serde_json::Value,
    ts: i64,
) -> String {
    let filter_label = friendly_filter_label(filter);
    let diag_line = condense_diag(diag);
    let when = if ts > 0 {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|t| t.with_timezone(&chrono_tz::Asia::Shanghai).format("%H:%M").to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let header = if name.is_empty() {
        format!("⚡ {code} — 触发 **{filter_label}**")
    } else {
        format!("⚡ {code} {name} — 触发 **{filter_label}**")
    };
    let mut out = header;
    if !when.is_empty() {
        out.push_str(&format!("  ({when})"));
    }
    if !diag_line.is_empty() {
        out.push_str("\n");
        out.push_str(&diag_line);
    }
    out
}

/// Map astock filter slug to a Chinese label. Unknown filters fall
/// back to the slug so the user still sees something.
fn friendly_filter_label(filter: &str) -> &'static str {
    match filter {
        "quick_rally" => "急涨",
        "quick_goldcross" => "金叉",
        "quick_deadtogold" => "死叉转金叉",
        "quick_cow_catch" => "抓牛股",
        _ => "异动",
    }
}

/// Condense the per-filter diag JSON into a single-line summary.
/// Best-effort: pluck the most useful fields when present, fall
/// back to truncated JSON otherwise.
fn condense_diag(diag: &serde_json::Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(o) = diag.as_object() {
        for (k, v) in o {
            let label = match k.as_str() {
                "pct" => "涨幅",
                "amount" => "成交额",
                "volume" => "成交量",
                "price" => "现价",
                "rsi" => "RSI",
                "macd" => "MACD",
                _ => continue,
            };
            if let Some(n) = v.as_f64() {
                if k == "pct" {
                    parts.push(format!("{label}: {n:+.2}%"));
                } else {
                    parts.push(format!("{label}: {n:.2}"));
                }
            } else if let Some(s) = v.as_str() {
                parts.push(format!("{label}: {s}"));
            }
        }
    }
    parts.join("  ·  ")
}

/// Walk the memory store for every watchlist entry whose `text` is
/// the given code. Returns `(channel, account, peer_id)` tuples —
/// the SSE bridge has no way to know which IM account the peer
/// originally talked through, so account stays `None` (the channel
/// sender map falls back to the bare channel key).
async fn collect_peers_watching_code(
    mem: &Arc<tokio::sync::Mutex<MemoryStore>>,
    code: &str,
) -> Vec<(String, Option<String>, String)> {
    let store = mem.lock().await;
    let docs = store.list_active();
    drop(store);
    let mut out: Vec<(String, Option<String>, String)> = Vec::new();
    for d in docs {
        if d.kind != "watchlist" {
            continue;
        }
        if d.text != code {
            continue;
        }
        // Scope: `agent:{id}:watchlist:{channel}:{peer}` — split on
        // ':' allowing peer_id to contain colons (matches the cron
        // briefing's reverse parser).
        let parts: Vec<&str> = d.scope.splitn(5, ':').collect();
        if parts.len() < 5 || parts[0] != "agent" || parts[2] != "watchlist" {
            continue;
        }
        out.push((parts[3].to_owned(), None, parts[4].to_owned()));
    }
    out
}

/// Tiny LRU keyed on dedupe strings with insertion-time-based
/// expiry. Designed for the dispatch path's single-thread access
/// pattern under a mutex — uses a Vec + linear scan because the
/// cap is small (~1024) and the hot operations are `contains` /
/// `insert` rather than ordered iteration.
struct DedupeLru {
    cap: usize,
    entries: HashMap<String, Instant>,
}

impl DedupeLru {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            entries: HashMap::with_capacity(cap),
        }
    }

    fn contains(&mut self, key: &str, now: Instant) -> bool {
        // Lazy GC: prune expired entries on every read so the map
        // doesn't grow forever.
        self.entries.retain(|_, t| now.duration_since(*t) < DEDUPE_WINDOW);
        self.entries.contains_key(key)
    }

    fn insert(&mut self, key: String, now: Instant) {
        if self.entries.len() >= self.cap {
            // Evict the oldest. Cheap enough at cap=1024.
            if let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest_key);
            }
        }
        self.entries.insert(key, now);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_filter_known_and_unknown() {
        assert_eq!(friendly_filter_label("quick_rally"), "急涨");
        assert_eq!(friendly_filter_label("quick_goldcross"), "金叉");
        assert_eq!(friendly_filter_label("quick_cow_catch"), "抓牛股");
        assert_eq!(friendly_filter_label("brand_new_filter"), "异动");
    }

    #[test]
    fn condense_diag_picks_known_fields() {
        let d = serde_json::json!({
            "pct": 7.43,
            "amount": 12345.67,
            "irrelevant": "ignored",
        });
        let s = condense_diag(&d);
        assert!(s.contains("涨幅: +7.43%"));
        assert!(s.contains("成交额: 12345.67"));
        assert!(!s.contains("ignored"));
    }

    #[test]
    fn condense_diag_handles_negative_pct() {
        let d = serde_json::json!({"pct": -2.10});
        let s = condense_diag(&d);
        assert!(s.contains("涨幅: -2.10%"));
    }

    #[test]
    fn condense_diag_empty_for_unknown_fields() {
        let d = serde_json::json!({"foo": "bar"});
        assert_eq!(condense_diag(&d), "");
    }

    #[test]
    fn format_hit_summary_full() {
        let d = serde_json::json!({"pct": 7.43});
        let s = format_hit_summary("quick_rally", "600519", "贵州茅台", &d, 1717_000_000);
        assert!(s.contains("600519"));
        assert!(s.contains("贵州茅台"));
        assert!(s.contains("急涨"));
        assert!(s.contains("+7.43%"));
    }

    #[test]
    fn format_hit_summary_missing_name() {
        let d = serde_json::Value::Null;
        let s = format_hit_summary("quick_rally", "600519", "", &d, 0);
        assert!(s.contains("600519"));
        assert!(!s.contains("  —"));
    }

    #[test]
    fn dedupe_lru_basic() {
        let mut d = DedupeLru::new(4);
        let t0 = Instant::now();
        assert!(!d.contains("a", t0));
        d.insert("a".into(), t0);
        assert!(d.contains("a", t0));
        // Strictly increasing timestamps so eviction order is
        // deterministic — production callers always insert with
        // monotonic `Instant::now()`, so ties are rare and the
        // eviction-on-tie behaviour is acceptable rather than tested.
        d.insert("b".into(), t0 + Duration::from_millis(1));
        d.insert("c".into(), t0 + Duration::from_millis(2));
        d.insert("d".into(), t0 + Duration::from_millis(3));
        d.insert("e".into(), t0 + Duration::from_millis(4));
        let probe = t0 + Duration::from_millis(5);
        assert!(!d.contains("a", probe), "oldest 'a' should be evicted");
        assert!(d.contains("b", probe));
        assert!(d.contains("e", probe));
    }

    #[test]
    fn dedupe_lru_window_expires() {
        let mut d = DedupeLru::new(4);
        let t0 = Instant::now();
        d.insert("a".into(), t0);
        // Inside the window — present.
        assert!(d.contains("a", t0 + Duration::from_secs(30)));
        // Past the window — gone.
        assert!(!d.contains("a", t0 + Duration::from_secs(61)));
    }
}
