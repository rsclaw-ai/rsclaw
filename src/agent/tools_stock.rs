//! Stock tools — A-share market data via the astock gateway.
//!
//! Five built-in tools surfaced to the LLM, all behind a single
//! "astock not configured → return structured note instead of
//! panicking" gate. None of them write state on rsclaw's side:
//! quotes and snapshots are read-through from astock; watchlist
//! changes (a write-side concern) belong in a follow-up.
//!
//! Tool naming convention (snake_case underscored) matches the
//! existing `knowledge_base` / `memory` / `web_browser` tools so
//! the vendor-side regex `^[a-zA-Z_][a-zA-Z0-9_]*$` keeps holding.

use anyhow::Result;
use serde_json::{Value, json};

use super::runtime::AgentRuntime;

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
    fn dormant_note_shape() {
        let v = astock_dormant_note();
        assert_eq!(v["ok"].as_bool(), Some(false));
        assert_eq!(v["code"].as_str(), Some("astock_not_configured"));
        assert!(v["error"].as_str().unwrap().contains("astock.enabled"));
    }
}
