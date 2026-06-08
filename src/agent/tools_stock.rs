//! Stock tools — A-share market data via the astock gateway.
//!
//! Six built-in tools surfaced to the LLM, all behind a single
//! "astock not configured → return structured note instead of
//! panicking" gate. Five of them (quote/kline/snapshot/ask/query)
//! are read-through; `stock_chart` writes a PNG artifact;
//! `stock_watchlist` writes to the memory store (peer-scoped facts
//! pinned against decay).
//!
//! Tool naming convention (snake_case underscored) matches the
//! existing `knowledge_base` / `memory` / `web_browser` tools so
//! the vendor-side regex `^[a-zA-Z_][a-zA-Z0-9_]*$` keeps holding.

use anyhow::Result;
use serde_json::{Value, json};

use super::runtime::{AgentRuntime, RunContext};

/// Cap on JSON output a single tool call returns to the LLM. Snapshot
/// responses can carry 5000+ rows on a busy day — pushing all of them
/// to the model context once per turn would blow past the token
/// budget on 9B-class models. 50 rows is enough for "今天涨幅最大
/// 的票" / watchlist-style asks; the caller can pass `limit > 50` to
/// override for deliberate full-market scans.
const DEFAULT_SNAPSHOT_LIMIT: usize = 50;
const MAX_SNAPSHOT_LIMIT: usize = 5000;

/// Iwencai answers can carry tens of KB of structured detail (sub-
/// listings, peer rankings, sentiment series). LLMs don't need the
/// long tail; truncate the raw response text to keep tool_result
/// from dominating the next prompt.
const ASK_TEXT_CHAR_CAP: usize = 12_000;

impl AgentRuntime {
    // ----- tool: stock_quote -----------------------------------------

    /// `stock_quote` — one or many real-time quotes.
    /// args: `{ code | codes: [string], format: "raw"|"summary"=summary }`.
    pub(crate) async fn tool_stock_quote(&self, args: Value) -> Result<Value> {
        let arc = match crate::astock::global_client() {
            Some(c) => c,
            None => return Ok(astock_dormant_note()),
        };
        let codes = extract_codes(&args);
        if codes.is_empty() {
            return Ok(json!({
                "ok": false,
                "error": "no code(s) provided — pass `code` (string) or `codes` (array of strings)"
            }));
        }
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("summary");
        let quotes = if codes.len() == 1 {
            match arc.quote(&codes[0]).await {
                Ok(q) => vec![q],
                Err(e) => {
                    return Ok(json!({
                        "ok": false,
                        "error": format!("astock quote: {e}"),
                        "codes": codes,
                    }));
                }
            }
        } else {
            match arc.quote_batch(&codes).await {
                Ok(qs) => qs,
                Err(e) => {
                    return Ok(json!({
                        "ok": false,
                        "error": format!("astock quote_batch: {e}"),
                        "codes": codes,
                    }));
                }
            }
        };
        let items: Vec<Value> = quotes.iter().map(|q| quote_to_json(q, format)).collect();
        Ok(json!({
            "ok": true,
            "count": items.len(),
            "quotes": items,
        }))
    }

    // ----- tool: stock_kline -----------------------------------------

    /// `stock_kline` — historical K-line bars.
    /// args: `{ code, period?: "1d"|"1m"|..., count?: int<=800,
    ///          offset?: int, adjust?: "none"|"qfq"|"hfq" }`.
    pub(crate) async fn tool_stock_kline(&self, args: Value) -> Result<Value> {
        let arc = match crate::astock::global_client() {
            Some(c) => c,
            None => return Ok(astock_dormant_note()),
        };
        let code = match args.get("code").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                return Ok(json!({
                    "ok": false,
                    "error": "`code` (string) is required"
                }));
            }
        };
        let period = args.get("period").and_then(Value::as_str);
        let count = args.get("count").and_then(Value::as_u64).map(|n| n as u32);
        let offset = args.get("offset").and_then(Value::as_u64).map(|n| n as u32);
        let adjust = args.get("adjust").and_then(Value::as_str);
        match arc.kline(code, period, count, offset, adjust).await {
            Ok(r) => Ok(json!({
                "ok": true,
                "code": r.code,
                "period": r.period,
                "adjust": r.adjust,
                "adjust_warning": r.adjust_warning,
                "bars": r.klines,
                "data_quality": r.data_quality,
            })),
            Err(e) => Ok(json!({
                "ok": false,
                "error": format!("astock kline: {e}"),
            })),
        }
    }

    // ----- tool: stock_snapshot --------------------------------------

    /// `stock_snapshot` — market snapshot, filtered + capped to keep
    /// the LLM's context window healthy.
    /// args: `{ ts?: string, market?: "SH"|"SZ"|"BJ", codes?: [string],
    ///          adjust?: "none"|"qfq"|"hfq", limit?: int<=5000,
    ///          sort_by?: "amount"|"pct"|"price" (default amount),
    ///          order?: "desc"|"asc" (default desc) }`.
    pub(crate) async fn tool_stock_snapshot(&self, args: Value) -> Result<Value> {
        let arc = match crate::astock::global_client() {
            Some(c) => c,
            None => return Ok(astock_dormant_note()),
        };
        let ts = args.get("ts").and_then(Value::as_str);
        let market = args.get("market").and_then(Value::as_str);
        let codes = extract_codes(&args);
        let adjust = args.get("adjust").and_then(Value::as_str);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_SNAPSHOT_LIMIT)
            .min(MAX_SNAPSHOT_LIMIT);
        let sort_by = args
            .get("sort_by")
            .and_then(Value::as_str)
            .unwrap_or("amount");
        let order = args.get("order").and_then(Value::as_str).unwrap_or("desc");
        let mut rows = match arc.snapshot(ts, market, &codes, adjust).await {
            Ok(rs) => rs,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "error": format!("astock snapshot: {e}"),
                }));
            }
        };
        let total = rows.len();
        rows.sort_by(|a, b| {
            let av = snapshot_sort_key(a, sort_by);
            let bv = snapshot_sort_key(b, sort_by);
            let ord = av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal);
            if order == "asc" { ord } else { ord.reverse() }
        });
        rows.truncate(limit);
        Ok(json!({
            "ok": true,
            "total": total,
            "shown": rows.len(),
            "sort_by": sort_by,
            "order": order,
            "rows": rows,
        }))
    }

    // ----- tool: stock_ask -------------------------------------------

    /// `stock_ask` — natural-language query via iwencai.
    /// PREFERRED over `stock_snapshot` / a hand-crafted filter when
    /// the user asks anything in human form ("今天涨停的科技股有哪些",
    /// "北向资金净流入前 20", "最近一周创业板连板高度榜"). astock
    /// proxies the heavy lifting to iwencai (同花顺), which handles
    /// the parsing and returns structured results.
    pub(crate) async fn tool_stock_ask(&self, args: Value) -> Result<Value> {
        let arc = match crate::astock::global_client() {
            Some(c) => c,
            None => return Ok(astock_dormant_note()),
        };
        let q = match args.get("query").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                return Ok(json!({
                    "ok": false,
                    "error": "`query` (string, natural language) is required"
                }));
            }
        };
        let page = args.get("page").and_then(Value::as_u64).map(|n| n as u32);
        let limit = args.get("limit").and_then(Value::as_u64).map(|n| n as u32);
        let call_type = args.get("call_type").and_then(Value::as_str);
        match arc.ask(q, page, limit, call_type).await {
            Ok(resp) => Ok(json!({
                "ok": true,
                "query": q,
                "response": truncate_ask_response(resp),
            })),
            Err(e) => Ok(json!({
                "ok": false,
                "error": format!("astock ask: {e}"),
            })),
        }
    }

    // ----- tool: stock_query -----------------------------------------

    /// `stock_query` — read-only SQL against astock's DuckDB.
    /// Power-user escape hatch when iwencai / ask can't express the
    /// query. astock validates the SQL server-side; we pass through.
    pub(crate) async fn tool_stock_query(&self, args: Value) -> Result<Value> {
        let arc = match crate::astock::global_client() {
            Some(c) => c,
            None => return Ok(astock_dormant_note()),
        };
        let sql = match args.get("sql").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                return Ok(json!({
                    "ok": false,
                    "error": "`sql` (string) is required (must be a read-only SELECT)"
                }));
            }
        };
        match arc.query_sql(sql).await {
            Ok(r) => Ok(json!({
                "ok": true,
                "columns": r.columns,
                "row_count": r.rows.len(),
                "rows": r.rows,
            })),
            Err(e) => Ok(json!({
                "ok": false,
                "error": format!("astock query: {e}"),
            })),
        }
    }

    // ----- tool: stock_chart -----------------------------------------

    /// `stock_chart` — render a K-line + volume PNG for the user.
    ///
    /// Distinct from `stock_kline` on purpose: `stock_kline` returns
    /// raw bars for the LLM to ANALYSE; `stock_chart` returns a file
    /// the agent runtime auto-uploads to the IM channel for the USER
    /// to LOOK AT. The two are routinely called together — analyze
    /// first, then send the chart with commentary.
    ///
    /// Defaults to 60 daily bars + MA5/10/20/60 overlay + volume
    /// subplot, 红涨绿跌 colors, light background. Returns the
    /// `__send_file` envelope the agent runtime recognises (same
    /// convention as the workspace `send_file` tool).
    pub(crate) async fn tool_stock_chart(&self, args: Value) -> Result<Value> {
        let arc = match crate::astock::global_client() {
            Some(c) => c,
            None => return Ok(astock_dormant_note()),
        };
        let code = match args.get("code").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.trim().to_owned(),
            _ => {
                return Ok(json!({
                    "ok": false,
                    "error": "`code` (string) is required"
                }));
            }
        };
        let period = args
            .get("period")
            .and_then(Value::as_str)
            .unwrap_or("1d")
            .to_owned();
        let count = args
            .get("count")
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .unwrap_or(60)
            .clamp(20, 200);
        let adjust = args
            .get("adjust")
            .and_then(Value::as_str)
            .unwrap_or("none");
        let ma_periods: Vec<usize> = args
            .get("ma")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as usize))
                    .filter(|p| (2..=120).contains(p))
                    .collect()
            })
            .unwrap_or_else(|| vec![5, 10, 20, 60]);
        let name_hint = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned);

        // Fetch bars + the latest quote in parallel — the quote
        // gives us a fresh price + change% for the title, which is
        // more meaningful than the last K-line's close (the close
        // is yesterday's number on daily bars during today's
        // trading session).
        let kline_fut = arc.kline(&code, Some(&period), Some(count), Some(0), Some(adjust));
        let quote_fut = arc.quote(&code);
        let (kline_res, quote_res) = tokio::join!(kline_fut, quote_fut);
        let kr = match kline_res {
            Ok(r) => r,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "error": format!("astock kline: {e}"),
                }));
            }
        };
        if kr.klines.is_empty() {
            return Ok(json!({
                "ok": false,
                "error": format!("astock returned 0 bars for {code} ({period})"),
            }));
        }
        let quote = quote_res.ok();

        // Title composition: `<name> <code>  ¥<price>  +X.XX%`.
        // Falls back to the latest K-line close when the live quote
        // didn't come back (after-hours / pytdx hiccup).
        let (price, change_pct) = if let Some(q) = &quote {
            (q.price, q.change_pct())
        } else {
            let last = kr.klines.last().unwrap();
            let prev = kr
                .klines
                .get(kr.klines.len().saturating_sub(2))
                .map(|b| b.close)
                .unwrap_or(last.close);
            let pct = if prev.abs() > f64::EPSILON {
                (last.close - prev) / prev * 100.0
            } else {
                0.0
            };
            (last.close, pct)
        };
        let display_name = name_hint
            .as_deref()
            .map(|n| format!("{n}  "))
            .unwrap_or_default();
        let title = format!(
            "{display_name}{code}  ¥{price:.2}  {sign}{pct:.2}%",
            sign = if change_pct >= 0.0 { "+" } else { "" },
            pct = change_pct,
        );
        let ma_str = ma_periods
            .iter()
            .map(|p| format!("MA{p}"))
            .collect::<Vec<_>>()
            .join("/");
        let subtitle = format!("{period}  ·  {n} 根  ·  {ma_str}", n = kr.klines.len());

        // Render to a fresh path under the rsclaw base var dir. We
        // do NOT reuse the artifact store here — that store is text-
        // only and per-session-keyed. Charts are short-lived files
        // for IM upload, so a flat ~/.rsclaw/var/charts/ with a uuid
        // name is the simplest sufficient layout.
        let charts_dir = crate::config::loader::base_dir().join("var").join("charts");
        let file_name = format!(
            "chart_{}_{}.png",
            code.replace(['.', '/', ':'], "_"),
            uuid::Uuid::new_v4().simple()
        );
        let path = charts_dir.join(&file_name);

        let opts = crate::astock::chart::ChartOpts {
            title: title.clone(),
            subtitle: Some(subtitle),
            ma_periods,
            ..Default::default()
        };
        let size = match crate::astock::chart::render_kline_png(&kr.klines, &opts, &path) {
            Ok(s) => s,
            Err(e) => {
                return Ok(json!({
                    "ok": false,
                    "error": format!("render_kline_png: {e:#}"),
                }));
            }
        };

        // `__send_file` envelope matches the existing workspace-file
        // send convention so the agent runtime auto-uploads to the
        // active IM channel without any extra plumbing.
        Ok(json!({
            "__send_file": true,
            "path": path.to_string_lossy(),
            "filename": file_name,
            "size": size,
            "summary": title,
        }))
    }

    // ----- tool: stock_watchlist -------------------------------------

    /// `stock_watchlist` — per-IM-user persistent stock list.
    ///
    /// Stored in the live memory store as docs with
    /// `scope = "agent:{agent_id}:watchlist:{channel}:{peer_id}"`,
    /// `kind = "watchlist"`, `pinned = true` (immune to tier decay —
    /// these are deliberate user choices, not learned facts). One doc
    /// per code so add/remove are atomic and the per-row tier doesn't
    /// matter.
    ///
    /// Actions:
    ///   * `list`  — return all codes currently on the watchlist.
    ///   * `add`   — `codes: [...]` adds N codes; dedupes against
    ///     existing entries (no duplicate docs).
    ///   * `remove` — `codes: [...]` removes by code. Missing codes
    ///     are silently skipped.
    ///   * `clear` — wipe the entire watchlist for this peer (rare;
    ///     used by reset / debug flows).
    pub(crate) async fn tool_stock_watchlist(
        &self,
        ctx: &RunContext,
        args: Value,
    ) -> Result<Value> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list")
            .trim();
        let Some(mem) = self.memory.as_ref() else {
            return Ok(json!({
                "ok": false,
                "code": "memory_unavailable",
                "error": "memory store not initialised — cannot persist watchlist"
            }));
        };
        let scope = watchlist_scope(&ctx.agent_id, &ctx.channel, &ctx.peer_id);
        match action {
            "list" => watchlist_list(mem, &scope).await,
            "add" => watchlist_add(mem, &scope, extract_codes(&args)).await,
            "remove" => watchlist_remove(mem, &scope, extract_codes(&args)).await,
            "clear" => watchlist_clear(mem, &scope).await,
            other => Ok(json!({
                "ok": false,
                "error": format!(
                    "unknown action '{other}'. Use list, add, remove, or clear."
                )
            })),
        }
    }
}

// --- module-level helpers (no `&self` needed) ----------------------------

fn extract_codes(args: &Value) -> Vec<String> {
    if let Some(s) = args.get("code").and_then(Value::as_str)
        && !s.trim().is_empty()
    {
        return vec![s.trim().to_owned()];
    }
    if let Some(arr) = args.get("codes").and_then(Value::as_array) {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    Vec::new()
}

fn quote_to_json(q: &crate::astock::Quote, format: &str) -> Value {
    let pct = q.change_pct();
    match format {
        "raw" => json!({
            "code": q.code,
            "market": q.market,
            "price": q.price,
            "open": q.open,
            "high": q.high,
            "low": q.low,
            "pre_close": q.pre_close,
            "volume": q.volume,
            "amount": q.amount,
            "bids": q.bids,
            "asks": q.asks,
            "change_pct": pct,
            "timestamp": q.timestamp,
        }),
        // "summary" (default) — trims order-book levels which the LLM
        // rarely uses and which inflate tokens.
        _ => json!({
            "code": q.code,
            "price": q.price,
            "change_pct": pct,
            "open": q.open,
            "high": q.high,
            "low": q.low,
            "pre_close": q.pre_close,
            "volume": q.volume,
            "amount": q.amount,
            "timestamp": q.timestamp,
        }),
    }
}

fn snapshot_sort_key(r: &crate::astock::SnapshotRow, key: &str) -> f64 {
    match key {
        "price" => r.price,
        "pct" => {
            if r.pre_close.abs() < f64::EPSILON {
                0.0
            } else {
                (r.price - r.pre_close) / r.pre_close * 100.0
            }
        }
        // "amount" default
        _ => r.amount,
    }
}

/// Iwencai responses can carry tens of KB of JSON (sub-listings,
/// alternative phrasings, peer rankings). Cap the longest string
/// fields to `ASK_TEXT_CHAR_CAP` characters so a single tool call
/// can't dominate the next prompt context.
fn truncate_ask_response(mut v: Value) -> Value {
    fn truncate_str(s: &mut String, cap: usize) {
        if s.chars().count() <= cap {
            return;
        }
        let truncated: String = s.chars().take(cap).collect();
        *s = format!("{truncated} …[truncated by rsclaw, {cap}-char cap]");
    }
    fn walk(v: &mut Value, cap: usize) {
        match v {
            Value::String(s) => truncate_str(s, cap),
            Value::Array(a) => {
                for item in a {
                    walk(item, cap);
                }
            }
            Value::Object(m) => {
                for (_k, vv) in m.iter_mut() {
                    walk(vv, cap);
                }
            }
            _ => {}
        }
    }
    walk(&mut v, ASK_TEXT_CHAR_CAP);
    v
}

/// Build the memory scope a single IM peer's watchlist lives under.
///
/// Each (agent, channel, peer) tuple gets its own scope so the same
/// gateway can serve multiple IM users without their watchlists
/// bleeding into each other. Format is hierarchical so a future
/// admin tool can list "every watchlist on this agent" via prefix
/// match.
pub(crate) fn watchlist_scope(agent_id: &str, channel: &str, peer_id: &str) -> String {
    format!("agent:{agent_id}:watchlist:{channel}:{peer_id}")
}

const WATCHLIST_KIND: &str = "watchlist";
/// Max codes a single watchlist can hold. Keeps `stock_snapshot
/// codes=[...watchlist...]` calls from accidentally batching a
/// thousand-code request that times out the upstream sidecar.
const WATCHLIST_CAP: usize = 50;

async fn watchlist_list(
    mem: &std::sync::Arc<tokio::sync::Mutex<crate::agent::memory::MemoryStore>>,
    scope: &str,
) -> Result<Value> {
    let store = mem.lock().await;
    let codes: Vec<String> = store
        .list_active()
        .into_iter()
        .filter(|d| d.scope == scope && d.kind == WATCHLIST_KIND)
        .map(|d| d.text)
        .collect();
    Ok(json!({
        "ok": true,
        "count": codes.len(),
        "codes": codes,
    }))
}

async fn watchlist_add(
    mem: &std::sync::Arc<tokio::sync::Mutex<crate::agent::memory::MemoryStore>>,
    scope: &str,
    raw_codes: Vec<String>,
) -> Result<Value> {
    if raw_codes.is_empty() {
        return Ok(json!({
            "ok": false,
            "error": "no codes provided — pass `code: \"600519\"` or `codes: [\"600519\", ...]`"
        }));
    }
    // Normalise + dedupe input (case-preserved, but trimmed). We
    // store the as-typed code so the user sees back what they
    // entered; normalise on read at the astock-client edge.
    let mut wanted: Vec<String> = raw_codes
        .into_iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    wanted.sort();
    wanted.dedup();

    let mut store = mem.lock().await;
    let existing: std::collections::HashSet<String> = store
        .list_active()
        .into_iter()
        .filter(|d| d.scope == scope && d.kind == WATCHLIST_KIND)
        .map(|d| d.text)
        .collect();
    let current_count = existing.len();
    let to_add: Vec<String> = wanted
        .into_iter()
        .filter(|c| !existing.contains(c))
        .collect();
    if current_count + to_add.len() > WATCHLIST_CAP {
        return Ok(json!({
            "ok": false,
            "error": format!(
                "watchlist cap reached: {current_count} existing + {} new > {WATCHLIST_CAP} max. \
                 Remove some codes first (action=remove) or raise the cap.",
                to_add.len()
            ),
            "cap": WATCHLIST_CAP,
            "current": current_count,
        }));
    }
    let mut added: Vec<String> = Vec::new();
    for code in to_add {
        let doc = crate::agent::memory::MemoryDoc {
            id: uuid::Uuid::new_v4().to_string(),
            scope: scope.to_owned(),
            kind: WATCHLIST_KIND.to_owned(),
            text: code.clone(),
            vector: vec![],
            created_at: 0,
            accessed_at: 0,
            access_count: 0,
            importance: 0.9,
            tier: Default::default(),
            abstract_text: None,
            overview_text: None,
            tags: vec!["stock_watchlist".to_owned()],
            // Pinned: user-curated, never decay, immune to crystallizer
            // demotion. Matches the contract for the memory note (per
            // `MemoryDoc.pinned` docs).
            pinned: true,
        };
        if let Err(e) = store.add(doc).await {
            return Ok(json!({
                "ok": false,
                "error": format!("watchlist add failed at {code}: {e:#}"),
                "added": added,
            }));
        }
        added.push(code);
    }
    Ok(json!({
        "ok": true,
        "added": added,
        "skipped_duplicates": current_count, // before-state count for context
        "total": existing.len() + added.len(),
    }))
}

async fn watchlist_remove(
    mem: &std::sync::Arc<tokio::sync::Mutex<crate::agent::memory::MemoryStore>>,
    scope: &str,
    raw_codes: Vec<String>,
) -> Result<Value> {
    if raw_codes.is_empty() {
        return Ok(json!({
            "ok": false,
            "error": "no codes provided"
        }));
    }
    let targets: std::collections::HashSet<String> = raw_codes
        .into_iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let mut store = mem.lock().await;
    let to_delete: Vec<String> = store
        .list_active()
        .into_iter()
        .filter(|d| d.scope == scope && d.kind == WATCHLIST_KIND && targets.contains(&d.text))
        .map(|d| d.id)
        .collect();
    let mut removed = 0usize;
    for id in &to_delete {
        if store.delete(id).await.is_ok() {
            removed += 1;
        }
    }
    Ok(json!({
        "ok": true,
        "removed": removed,
        "requested": targets.len(),
    }))
}

async fn watchlist_clear(
    mem: &std::sync::Arc<tokio::sync::Mutex<crate::agent::memory::MemoryStore>>,
    scope: &str,
) -> Result<Value> {
    let mut store = mem.lock().await;
    let to_delete: Vec<String> = store
        .list_active()
        .into_iter()
        .filter(|d| d.scope == scope && d.kind == WATCHLIST_KIND)
        .map(|d| d.id)
        .collect();
    let mut removed = 0usize;
    for id in &to_delete {
        if store.delete(id).await.is_ok() {
            removed += 1;
        }
    }
    Ok(json!({
        "ok": true,
        "removed": removed,
    }))
}

fn astock_dormant_note() -> Value {
    json!({
        "ok": false,
        "code": "astock_not_configured",
        "error": "astock subsystem is disabled. Tell the user to set \
                  `astock.enabled: true` and `astock.baseUrl` in \
                  ~/.rsclaw/rsclaw.json5 to enable A-share market data."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_codes_single_string() {
        let v = json!({"code": "600519"});
        assert_eq!(extract_codes(&v), vec!["600519".to_string()]);
    }

    #[test]
    fn extract_codes_array() {
        let v = json!({"codes": ["600519", "000001", " 300750 "]});
        assert_eq!(
            extract_codes(&v),
            vec!["600519".to_string(), "000001".to_string(), "300750".to_string()]
        );
    }

    #[test]
    fn extract_codes_array_ignores_empty_and_non_strings() {
        let v = json!({"codes": ["600519", "", 42, null, "  ", "000001"]});
        assert_eq!(
            extract_codes(&v),
            vec!["600519".to_string(), "000001".to_string()]
        );
    }

    #[test]
    fn extract_codes_none() {
        let v = json!({"unrelated": true});
        assert!(extract_codes(&v).is_empty());
    }

    #[test]
    fn snapshot_sort_key_paths() {
        let r = crate::astock::SnapshotRow {
            code: "X".into(),
            name: None,
            market: String::new(),
            price: 105.0,
            pre_close: 100.0,
            volume: 0,
            amount: 12_345_678.0,
            timestamp: 0,
            extra: Default::default(),
        };
        assert!((snapshot_sort_key(&r, "amount") - 12_345_678.0).abs() < 1e-3);
        assert!((snapshot_sort_key(&r, "price") - 105.0).abs() < 1e-6);
        assert!((snapshot_sort_key(&r, "pct") - 5.0).abs() < 1e-6);
        // Unknown key falls back to amount.
        assert!((snapshot_sort_key(&r, "garbage") - 12_345_678.0).abs() < 1e-3);
    }

    #[test]
    fn snapshot_sort_key_pct_handles_zero_preclose() {
        let r = crate::astock::SnapshotRow {
            code: "X".into(),
            name: None,
            market: String::new(),
            price: 10.0,
            pre_close: 0.0,
            volume: 0,
            amount: 0.0,
            timestamp: 0,
            extra: Default::default(),
        };
        assert_eq!(snapshot_sort_key(&r, "pct"), 0.0);
    }

    #[test]
    fn truncate_ask_response_caps_long_strings() {
        let huge = "x".repeat(ASK_TEXT_CHAR_CAP + 500);
        let v = json!({"text": huge.clone(), "nested": {"sub": huge}});
        let out = truncate_ask_response(v);
        let t = out["text"].as_str().unwrap();
        assert!(t.starts_with(&"x".repeat(ASK_TEXT_CHAR_CAP)));
        assert!(t.contains("truncated by rsclaw"));
        let s = out["nested"]["sub"].as_str().unwrap();
        assert!(s.contains("truncated by rsclaw"));
    }

    #[test]
    fn truncate_ask_response_preserves_short_strings() {
        let v = json!({"text": "short", "n": 1, "arr": [1, 2, 3]});
        let out = truncate_ask_response(v);
        assert_eq!(out["text"].as_str(), Some("short"));
        assert_eq!(out["n"].as_i64(), Some(1));
        assert_eq!(out["arr"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn watchlist_scope_format() {
        assert_eq!(
            watchlist_scope("main", "feishu", "ou_abc"),
            "agent:main:watchlist:feishu:ou_abc"
        );
        // Per (channel, peer) — same peer on a different channel gets
        // its own list. Important so the wechat / feishu watchlists
        // don't bleed.
        assert_ne!(
            watchlist_scope("main", "feishu", "x"),
            watchlist_scope("main", "wechat", "x")
        );
        // Prefix is stable so a future admin tool can `list_active`
        // and prefix-match `agent:main:watchlist:` to dump everything.
        assert!(watchlist_scope("main", "feishu", "x").starts_with("agent:main:watchlist:"));
    }

    #[test]
    fn dormant_note_shape() {
        let v = astock_dormant_note();
        assert_eq!(v["ok"].as_bool(), Some(false));
        assert_eq!(v["code"].as_str(), Some("astock_not_configured"));
        assert!(v["error"].as_str().unwrap().contains("astock.enabled"));
    }
}
