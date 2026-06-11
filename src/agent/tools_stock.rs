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
        let markdown = render_quotes_markdown(&quotes);
        Ok(json!({
            "ok": true,
            "count": items.len(),
            "quotes": items,
            // Pre-rendered IM-friendly text. Use this verbatim in chat
            // replies — it already carries +/- sign, 万/亿 units,
            // and aligns columns. Saves the LLM from re-formatting JSON
            // and keeps the rendering consistent across chats / models.
            "markdown": markdown,
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
            Ok(r) => {
                let markdown = render_kline_summary_markdown(&r);
                Ok(json!({
                    "ok": true,
                    "code": r.code,
                    "period": r.period,
                    "adjust": r.adjust,
                    "adjust_warning": r.adjust_warning,
                    "bars": r.klines,
                    "data_quality": r.data_quality,
                    "markdown": markdown,
                }))
            }
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
    ///          order?: "desc"|"asc" (default desc),
    ///          use_watchlist?: bool }`.
    ///
    /// Default watchlist behaviour: when `codes` is empty AND
    /// `market` is unset AND `ts` is unset AND the calling peer has
    /// a non-empty watchlist (via `stock_watchlist`), the snapshot
    /// auto-filters to those codes. Lets the user ask
    /// "我关注的股票今天怎么样" with a single tool call instead of
    /// `stock_watchlist list` → `stock_snapshot codes=[...]`.
    /// Set `use_watchlist=false` to force a full-market snapshot
    /// even when the watchlist is non-empty.
    pub(crate) async fn tool_stock_snapshot(
        &self,
        ctx: &RunContext,
        args: Value,
    ) -> Result<Value> {
        let arc = match crate::astock::global_client() {
            Some(c) => c,
            None => return Ok(astock_dormant_note()),
        };
        let ts = args.get("ts").and_then(Value::as_str);
        let market = args.get("market").and_then(Value::as_str);
        let mut codes = extract_codes(&args);
        let mut used_watchlist = false;
        // Watchlist auto-fill: only when the LLM hasn't passed any
        // explicit filter (no codes, no market, no historical ts).
        // Adding a market filter implies "I want the whole market",
        // so honour that and skip the watchlist substitution.
        let use_watchlist_opt = args.get("use_watchlist").and_then(Value::as_bool);
        let watchlist_eligible = codes.is_empty()
            && market.is_none()
            && ts.is_none()
            && use_watchlist_opt != Some(false);
        if watchlist_eligible && let Some(mem) = self.memory.as_ref() {
            let scope = watchlist_scope(&ctx.agent_id, &ctx.channel, &ctx.peer_id);
            let store = mem.lock().await;
            codes = store
                .list_active()
                .into_iter()
                .filter(|d| d.scope == scope && d.kind == WATCHLIST_KIND)
                .map(|d| d.text)
                .collect();
            drop(store);
            used_watchlist = !codes.is_empty();
        }
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
        let markdown = render_snapshot_markdown(&rows, used_watchlist, sort_by);
        Ok(json!({
            "ok": true,
            "total": total,
            "shown": rows.len(),
            "sort_by": sort_by,
            "order": order,
            // Tells the LLM whether it just got a watchlist-scoped
            "used_watchlist": used_watchlist,
            // snapshot or a market-wide one. Useful when composing
            // the user-facing reply ("你关注的 5 只股票:..."" vs
            // "全市场前 50:....""), and avoids the LLM having to
            // remember the call shape.
            "rows": rows,
            "markdown": markdown,
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
        // Default to qfq (前复权) — see ChartRenderInput rationale below.
        let adjust = args
            .get("adjust")
            .and_then(Value::as_str)
            .unwrap_or("qfq")
            .to_owned();
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

        match render_chart_for_code(ChartRenderInput {
            code: &code,
            period: &period,
            count,
            adjust: &adjust,
            ma_periods,
            name_hint: name_hint.as_deref(),
        })
        .await
        {
            Ok(out) => Ok(json!({
                "__send_file": true,
                "path": out.path.to_string_lossy(),
                "filename": out.filename,
                "size": out.size,
                "summary": out.title,
            })),
            Err(ChartRenderError::Dormant) => Ok(astock_dormant_note()),
            Err(ChartRenderError::Soft(msg)) => Ok(json!({
                "ok": false,
                "error": msg,
            })),
        }
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

// ---------------------------------------------------------------------------
// Chart render — free-function form used by `tool_stock_chart` and
// the `/astock chart` slash command. Kept here (not under
// `astock::chart`) because the rendering recipe (qfq default, MA list,
// live-quote-vs-bar honesty, subtitle text, file-path scheme) is part
// of the public chart "look" — the `astock::chart` module owns the
// low-level rasterisation primitive only.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct ChartRenderInput<'a> {
    pub code: &'a str,
    pub period: &'a str,
    pub count: u32,
    pub adjust: &'a str,
    pub ma_periods: Vec<usize>,
    pub name_hint: Option<&'a str>,
}

#[derive(Debug)]
pub(crate) struct ChartRenderOutput {
    pub path: std::path::PathBuf,
    pub filename: String,
    pub title: String,
    pub size: u64,
}

#[derive(Debug)]
pub(crate) enum ChartRenderError {
    /// astock is not configured (mirrors `astock_dormant_note`).
    Dormant,
    /// User-actionable error message — bad code, empty bars, render
    /// failure, etc. Caller picks the surface (JSON for tool, plain
    /// text for slash).
    Soft(String),
}

pub(crate) async fn render_chart_for_code(
    input: ChartRenderInput<'_>,
) -> std::result::Result<ChartRenderOutput, ChartRenderError> {
    let arc = match crate::astock::global_client() {
        Some(c) => c,
        None => return Err(ChartRenderError::Dormant),
    };
    let code = input.code.to_owned();
    let period = input.period;
    let count = input.count.clamp(20, 200);
    let adjust = input.adjust;
    let ma_periods = input.ma_periods;
    // Auto-fill the chart title's name when the caller didn't provide
    // one — the resolver hits the same name_cache the rest of the
    // /astock surface uses, so subsequent charts in the same minute
    // are free.
    let resolved_name: Option<String> = if input.name_hint.is_some() {
        None
    } else {
        let map = arc.resolve_names(std::slice::from_ref(&code)).await;
        map.get(&code).cloned()
    };
    let name_hint: Option<&str> = input
        .name_hint
        .or(resolved_name.as_deref());

    // Fetch bars + the latest quote in parallel — the quote gives us
    // a fresh price + change% for the title, more meaningful than
    // the last K-line's close (the close is yesterday's number on
    // daily bars during today's trading session).
    let kline_fut = arc.kline(&code, Some(period), Some(count), Some(0), Some(adjust));
    let quote_fut = arc.quote(&code);
    let (kline_res, quote_res) = tokio::join!(kline_fut, quote_fut);
    let kr = match kline_res {
        Ok(r) => r,
        Err(e) => return Err(ChartRenderError::Soft(format!("astock kline: {e}"))),
    };
    if kr.klines.is_empty() {
        return Err(ChartRenderError::Soft(format!(
            "astock returned 0 bars for {code} ({period})"
        )));
    }
    let quote = quote_res.ok();

    // Title price source: prefer the live quote (it shows what the
    // stock is doing NOW), but ONLY when the live quote's calendar
    // day matches the last K-line bar's day. The first cut always
    // used the live quote — which produced charts where the title
    // said "¥1272.86 +0.38%" while the visible last candle was 10
    // days old at ¥1270, looking like a data mismatch even though
    // both numbers were correct. Now: when the live quote is from
    // a day that ISN'T plotted, we fall back to the last K-line
    // bar's close + the bar-over-bar change. Honesty over freshness
    // for the headline number.
    let last_bar = kr.klines.last().unwrap();
    let last_bar_day = chrono::DateTime::from_timestamp(last_bar.timestamp, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    let live_quote_is_today_bar = match &quote {
        Some(q) => {
            let qd = chrono::DateTime::from_timestamp(q.timestamp, 0)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            !qd.is_empty() && qd == last_bar_day
        }
        None => false,
    };
    let (price, change_pct) = if live_quote_is_today_bar {
        let q = quote.as_ref().unwrap();
        (q.price, q.change_pct())
    } else {
        let prev = kr
            .klines
            .get(kr.klines.len().saturating_sub(2))
            .map(|b| b.close)
            .unwrap_or(last_bar.close);
        let pct = if prev.abs() > f64::EPSILON {
            (last_bar.close - prev) / prev * 100.0
        } else {
            0.0
        };
        (last_bar.close, pct)
    };
    let display_name = name_hint
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
    // Subtitle: period · N 根 · 截至 date · adjust state · MA list.
    // The adjust state is REAL — if the caller asked for qfq but
    // astock downgraded to none (no factor table for the code), we
    // surface that in the subtitle so the user understands why the
    // chart shows a price gap.
    let adjust_label = match (kr.adjust.as_str(), kr.adjust_warning.as_deref()) {
        ("qfq", _) => "前复权",
        ("hfq", _) => "后复权",
        ("none", Some(_)) => "未复权 ⚠",
        ("none", None) => "未复权",
        (other, _) => other,
    };
    let subtitle = format!(
        "{period}  ·  {n} 根  ·  截至 {last_bar_day}  ·  {adjust_label}  ·  {ma_str}",
        n = kr.klines.len()
    );

    let charts_dir = crate::config::loader::base_dir().join("var").join("charts");
    let filename = format!(
        "chart_{}_{}.png",
        code.replace(['.', '/', ':'], "_"),
        uuid::Uuid::new_v4().simple()
    );
    let path = charts_dir.join(&filename);

    let opts = crate::astock::chart::ChartOpts {
        title: title.clone(),
        subtitle: Some(subtitle),
        ma_periods,
        ..Default::default()
    };
    let size = crate::astock::chart::render_kline_png(&kr.klines, &opts, &path)
        .map_err(|e| ChartRenderError::Soft(format!("render_kline_png: {e:#}")))?;

    Ok(ChartRenderOutput {
        path,
        filename,
        title,
        size,
    })
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

pub(crate) async fn watchlist_list(
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

pub(crate) async fn watchlist_add(
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

pub(crate) async fn watchlist_remove(
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

pub(crate) async fn watchlist_clear(
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

// ---------------------------------------------------------------------------
// IM-friendly markdown rendering
// ---------------------------------------------------------------------------
//
// Each stock_* tool returns a `markdown` field next to its JSON. Goals:
//
// * **Survive every IM client** — no emoji that renders differently in
//   feishu/wechat/telegram-with-default-font. Plain ASCII signs and CJK
//   numerals (`万` / `亿`).
// * **Preserve the 红涨绿跌 convention** — but in plain text, since IM
//   markdown doesn't reliably colorize. Use sign + leading character
//   (`+2.35%` / `-1.20%`) and let the user's brain do the rest.
// * **Compact** — one line per quote in batch, ≤4 columns in tables.
//   Chat windows are narrow; lines longer than ~40 CJK chars wrap ugly.
// * **No agent re-formatting needed** — the LLM should echo this field
//   verbatim. Saves tokens, avoids 9B-models mangling the precision.

/// Format a single quote as a one-line summary.
///
/// Shape: `<code>  ¥<price>  <+|-N.NN%>  额<money>  量<volume>`
/// Example: `600519  ¥1272.86  +0.07%  额39.84亿  量313.04万手`
fn render_one_quote_line(
    q: &crate::astock::Quote,
    names: Option<&std::collections::HashMap<String, String>>,
) -> String {
    let pct = q.change_pct();
    let sign = if pct >= 0.0 { "+" } else { "" };
    let name = names
        .and_then(|m| m.get(&q.code))
        .map(String::as_str)
        .unwrap_or("");
    let head = if name.is_empty() {
        q.code.clone()
    } else {
        format!("{} {}", q.code, name)
    };
    format!(
        "{}  ¥{:.2}  {}{:.2}%  额{}  量{}",
        head,
        q.price,
        sign,
        pct,
        compact_cny(q.amount),
        compact_volume(q.volume),
    )
}

pub(crate) fn render_quotes_markdown(quotes: &[crate::astock::Quote]) -> String {
    render_quotes_markdown_with_names(quotes, None)
}

/// Same shape as `render_quotes_markdown` but accepts an optional
/// code → name lookup so callers that already resolved names (e.g.
/// the `/astock quote` slash handler) can render `600519 贵州茅台`
/// instead of `600519` alone.
pub(crate) fn render_quotes_markdown_with_names(
    quotes: &[crate::astock::Quote],
    names: Option<&std::collections::HashMap<String, String>>,
) -> String {
    if quotes.is_empty() {
        return "_没有报价数据_".to_owned();
    }
    if quotes.len() == 1 {
        return render_one_quote_line(&quotes[0], names);
    }
    quotes
        .iter()
        .map(|q| format!("- {}", render_one_quote_line(q, names)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Snapshot rendered as a markdown table. Truncates the row count to
/// `MAX_MARKDOWN_ROWS` so a 5000-row snapshot doesn't carry a 5000-row
/// table into the next IM message. The JSON `rows` field still carries
/// the full data for any LLM that wants to scan.
pub(crate) fn render_snapshot_markdown(
    rows: &[crate::astock::SnapshotRow],
    used_watchlist: bool,
    sort_by: &str,
) -> String {
    const MAX_MARKDOWN_ROWS: usize = 20;
    if rows.is_empty() {
        if used_watchlist {
            return "_关注列表里没有有效报价 — 可能没设置或市场未开盘_".to_owned();
        }
        return "_无快照数据_".to_owned();
    }
    let header = if used_watchlist {
        format!("**关注列表** ({} 只, 按 {sort_by})", rows.len())
    } else {
        format!("**全市场** ({} 行, 按 {sort_by})", rows.len())
    };
    let mut out = String::new();
    out.push_str(&header);
    out.push_str("\n\n");
    out.push_str("| 代码 | 名称 | 现价 | 涨跌幅 | 成交额 |\n");
    out.push_str("|---|---|---:|---:|---:|\n");
    for r in rows.iter().take(MAX_MARKDOWN_ROWS) {
        let pct = if r.pre_close.abs() > f64::EPSILON {
            (r.price - r.pre_close) / r.pre_close * 100.0
        } else {
            0.0
        };
        let sign = if pct >= 0.0 { "+" } else { "" };
        out.push_str(&format!(
            "| {} | {} | {:.2} | {}{:.2}% | {} |\n",
            r.code,
            r.name.as_deref().unwrap_or(""),
            r.price,
            sign,
            pct,
            compact_cny(r.amount),
        ));
    }
    if rows.len() > MAX_MARKDOWN_ROWS {
        out.push_str(&format!("\n_... 共 {} 行,显示前 {}_", rows.len(), MAX_MARKDOWN_ROWS));
    }
    out
}

/// K-line summary stats for the period covered. The chart itself
/// gets delivered via `stock_chart`; this block is the textual
/// commentary that should accompany the chart.
fn render_kline_summary_markdown(r: &crate::astock::KlineResp) -> String {
    if r.klines.is_empty() {
        return "_无 K 线数据_".to_owned();
    }
    let first = r.klines.first().unwrap();
    let last = r.klines.last().unwrap();
    let total_pct = if first.close.abs() > f64::EPSILON {
        (last.close - first.close) / first.close * 100.0
    } else {
        0.0
    };
    let mut hi = f64::NEG_INFINITY;
    let mut lo = f64::INFINITY;
    let mut max_vol: i64 = 0;
    for b in &r.klines {
        if b.high > hi {
            hi = b.high;
        }
        if b.low < lo {
            lo = b.low;
        }
        if b.volume > max_vol {
            max_vol = b.volume;
        }
    }
    let sign = if total_pct >= 0.0 { "+" } else { "" };
    let warn_line = match &r.adjust_warning {
        Some(w) => format!("\n⚠️ 复权降级 — {w}"),
        None => String::new(),
    };
    format!(
        "**{}**  {}  ({} 根, {})\n\n\
         - 最新: ¥{:.2}  ({sign}{:.2}% vs 区间起点)\n\
         - 区间: ¥{:.2} - ¥{:.2}\n\
         - 最大成交: {}{}",
        r.code,
        r.period,
        r.klines.len(),
        if r.adjust == "none" { "未复权" } else { &r.adjust },
        last.close,
        total_pct,
        lo,
        hi,
        compact_volume(max_vol),
        warn_line,
    )
}

/// Format CNY in 万 / 亿 with one decimal place. astock returns yuan
/// (元), so 万 = 1e4, 亿 = 1e8. Below 万 falls back to plain integer.
fn compact_cny(yuan: f64) -> String {
    let abs = yuan.abs();
    if abs >= 1e8 {
        format!("{:.2}亿", yuan / 1e8)
    } else if abs >= 1e4 {
        format!("{:.2}万", yuan / 1e4)
    } else {
        format!("{:.0}", yuan)
    }
}

/// Format share volume in 万 / 亿 手. astock returns volume in lots
/// ("手" — 1 lot = 100 shares on A-share). Same scale as CNY: above
/// 1e4 lots = 万手, above 1e8 = 亿手.
fn compact_volume(vol: i64) -> String {
    let abs = vol.unsigned_abs();
    if abs >= 100_000_000 {
        format!("{:.2}亿手", vol as f64 / 1e8)
    } else if abs >= 10_000 {
        format!("{:.2}万手", vol as f64 / 1e4)
    } else {
        format!("{vol}手")
    }
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
    fn compact_cny_thresholds() {
        assert_eq!(compact_cny(0.0), "0");
        assert_eq!(compact_cny(9_999.0), "9999");
        assert_eq!(compact_cny(12_345.0), "1.23万");
        assert_eq!(compact_cny(12_345_678_000.0), "123.46亿");
    }

    #[test]
    fn compact_volume_thresholds() {
        assert_eq!(compact_volume(0), "0手");
        assert_eq!(compact_volume(9_999), "9999手");
        assert_eq!(compact_volume(123_456), "12.35万手");
        assert_eq!(compact_volume(123_456_789_012), "1234.57亿手");
    }

    #[test]
    fn render_one_quote_line_shape() {
        let q = crate::astock::Quote {
            code: "600519".into(),
            market: String::new(),
            price: 1272.86,
            open: 1271.0,
            high: 1283.0,
            low: 1267.74,
            pre_close: 1272.0,
            volume: 3_130_400,
            amount: 3_984_001_792.0,
            bids: vec![],
            asks: vec![],
            timestamp: 0,
        };
        let s = render_one_quote_line(&q);
        assert!(s.contains("600519"));
        assert!(s.contains("¥1272.86"));
        assert!(s.contains("+"), "positive change should carry leading +");
        assert!(s.contains("亿"), "amount in billions");
        assert!(s.contains("万手"), "volume in 万手");
    }

    #[test]
    fn render_quotes_markdown_single_vs_batch() {
        let q1 = crate::astock::Quote {
            code: "600519".into(),
            market: String::new(),
            price: 100.0,
            open: 100.0,
            high: 100.0,
            low: 100.0,
            pre_close: 100.0,
            volume: 0,
            amount: 0.0,
            bids: vec![],
            asks: vec![],
            timestamp: 0,
        };
        let q2 = crate::astock::Quote {
            code: "000001".into(),
            ..q1.clone()
        };
        let single = render_quotes_markdown(&[q1.clone()]);
        assert!(!single.starts_with('-'), "single quote should NOT bullet");
        let batch = render_quotes_markdown(&[q1, q2]);
        assert!(batch.starts_with("- "), "batch quotes should bullet");
        assert!(batch.contains("600519"));
        assert!(batch.contains("000001"));
    }

    #[test]
    fn render_snapshot_markdown_used_watchlist() {
        let rows = vec![crate::astock::SnapshotRow {
            code: "600519".into(),
            name: Some("贵州茅台".into()),
            market: "SH".into(),
            price: 1272.86,
            pre_close: 1272.0,
            volume: 0,
            amount: 3_984_001_792.0,
            timestamp: 0,
            extra: Default::default(),
        }];
        let md = render_snapshot_markdown(&rows, true, "amount");
        assert!(md.contains("关注列表"));
        assert!(md.contains("600519"));
        assert!(md.contains("贵州茅台"));
    }

    #[test]
    fn render_snapshot_empty_watchlist_message() {
        let md = render_snapshot_markdown(&[], true, "amount");
        assert!(md.contains("关注列表"));
        assert!(md.contains("没设置或市场未开盘"));
    }

    #[test]
    fn render_kline_summary_carries_range_and_change() {
        use crate::astock::{KlineBar, KlineResp};
        let kr = KlineResp {
            code: "600519".into(),
            period: "1d".into(),
            adjust: "none".into(),
            adjust_warning: None,
            klines: vec![
                KlineBar { timestamp: 1, open: 100.0, high: 102.0, low: 99.0, close: 100.0, volume: 1000, amount: 0.0 },
                KlineBar { timestamp: 2, open: 100.0, high: 110.0, low: 99.5, close: 110.0, volume: 5000, amount: 0.0 },
            ],
            data_quality: serde_json::Value::Null,
        };
        let md = render_kline_summary_markdown(&kr);
        assert!(md.contains("600519"));
        assert!(md.contains("1d"));
        assert!(md.contains("+10.00%"), "open-to-close change %");
        assert!(md.contains("99.00"), "low");
        assert!(md.contains("110.00"), "high");
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
