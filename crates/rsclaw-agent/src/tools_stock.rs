//! Stock market tools for rsclaw agent.
//!
//! Integrates astock-core functionality into rsclaw's tool system:
//! - `stock_quote`: Real-time stock quotes via HTTP (Eastmoney/Tencent/Sina)
//! - `stock_select`: Post-market stock selection via Tushare (requires token)

use anyhow::{Result, Context, bail};
use serde_json::{Value, json};

use super::stock_capability::{RsclawCdp, RsclawConfig, create_http_client};
use astock_core::StockEngine;
use astock_core::capability::ConfigProvider;
use astock_core::algo::selector::SelectionStrategy;

// ============================================================================
// Tool Definitions
// ============================================================================

/// Tool definitions for stock tools.
pub fn stock_tool_defs() -> Vec<rsclaw_provider::ToolDef> {
    vec![
        rsclaw_provider::ToolDef {
            name: "stock_quote".to_owned(),
            description: "Fetch real-time stock quotes via HTTP (no token required). \
                Data sources: Eastmoney → Tencent → Sina (auto fallback). \
                Input: one or more stock codes (e.g., '600519.SH', '000001.SZ'). \
                Returns: current price, change %, volume, turnover rate, market cap. \
                Use this when the user asks about current stock prices.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "codes": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Stock codes in ts_code format, e.g., ['600519.SH', '000001.SZ']"
                    }
                },
                "required": ["codes"]
            }),
        },
        rsclaw_provider::ToolDef {
            name: "stock_select".to_owned(),
            description: "Post-market stock selection based on quantitative strategy. \
                Filters A-shares by: turnover rate, market cap, price change, etc. \
                Data source: Tushare (requires tushare_token in rsclaw.json5). \
                Use this for: '选股', '筛选股票', '找出符合条件的股票'.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "trade_date": {
                        "type": "string",
                        "description": "Trade date in YYYY-MM-DD format. Default: latest trade date."
                    },
                    "turnover_rate_min": {
                        "type": "number",
                        "description": "Minimum turnover rate (%). Default: 3."
                    },
                    "market_cap_max_yi": {
                        "type": "number",
                        "description": "Maximum circulating market value (亿元). Default: 100."
                    },
                    "change_pct_min": {
                        "type": "number",
                        "description": "Minimum price change (%). Default: 0 (no filter)."
                    },
                    "change_pct_max": {
                        "type": "number",
                        "description": "Maximum price change (%). Default: 10 (exclude extremes)."
                    },
                    "select_count": {
                        "type": "integer",
                        "description": "Maximum stocks to return. Default: 20."
                    }
                }
            }),
        },
    ]
}

// ============================================================================
// Tool Handlers
// ============================================================================

/// Build a StockEngine with available capabilities.
fn build_engine() -> Result<StockEngine> {
    let http = create_http_client()
        .context("Failed to create HTTP client for stock engine")?;

    let config = RsclawConfig::new();

    StockEngine::builder()
        .http(http)  // Direct value, not Arc
        .cdp(RsclawCdp::new())
        .llm(astock_core::capability::NoLlm)
        .config(config)  // Direct value, not Arc
        .build()
        .context("Failed to build StockEngine")
}

/// Handle stock_quote tool call.
/// Uses Eastmoney/Tencent/Sina HTTP APIs - no tushare_token required.
pub async fn handle_stock_quote(args: &Value) -> Result<Value> {
    let codes: Vec<String> = args["codes"]
        .as_array()
        .context("codes must be an array")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if codes.is_empty() {
        bail!("codes array is empty");
    }

    let engine = build_engine()?;

    // Convert codes to references
    let code_refs: Vec<&str> = codes.iter().map(|s| s.as_str()).collect();

    let quotes = engine.realtime_quotes(&code_refs).await?;

    // Format as JSON
    let result: Vec<Value> = quotes
        .into_iter()
        .map(|q| json!({
            "code": q.ts_code,
            "name": q.name,
            "price": q.price,
            "change_pct": q.change_pct,
            "volume": q.volume,
            "amount": q.amount,
            "turnover_rate": q.turnover_rate,
            "circ_mv": q.circ_mv,
            "total_mv": q.total_mv,
        }))
        .collect();

    Ok(json!({
        "success": true,
        "source": "eastmoney/tencent/sina HTTP",
        "quotes": result,
        "count": result.len()
    }))
}

/// Handle stock_select tool call.
/// Uses Tushare API - requires tushare_token in rsclaw.json5.
pub async fn handle_stock_select(args: &Value) -> Result<Value> {
    // Check tushare token first
    let config = RsclawConfig::new();
    if config.tushare_token().is_none() {
        bail!("stock_select requires tushare_token. \
            Add 'tushare_token' or 'astock.tushare_token' to rsclaw.json5.\n\
            Get your token from https://tushare.pro");
    }

    let engine = build_engine()?;

    // Build strategy from args - use correct field names
    let mut strategy = SelectionStrategy::default();

    if let Some(v) = args.get("turnover_rate_min") {
        if let Some(n) = v.as_f64() {
            strategy.turnover_rate_min = Some(n);
        }
    }
    if let Some(v) = args.get("market_cap_max_yi") {
        if let Some(n) = v.as_f64() {
            strategy.market_cap_max_yi = Some(n);
        }
    }
    if let Some(v) = args.get("change_pct_min") {
        if let Some(n) = v.as_f64() {
            strategy.change_pct_min = Some(n);
        }
    }
    if let Some(v) = args.get("change_pct_max") {
        if let Some(n) = v.as_f64() {
            strategy.change_pct_max = Some(n);
        }
    }
    if let Some(v) = args.get("select_count") {
        if let Some(n) = v.as_i64() {
            strategy.select_count = Some(n as usize);
        }
    }

    let report = if let Some(date) = args.get("trade_date").and_then(|v| v.as_str()) {
        engine.select_on_date(&strategy, date).await?
    } else {
        engine.select(&strategy).await?
    };

    // Return pre-rendered markdown plus structured data
    // Use correct field names from SelectedStock
    Ok(json!({
        "success": true,
        "source": "tushare",
        "markdown": report.markdown,
        "trade_date": report.trade_date,
        "count": report.stocks.len(),
        "stocks": report.stocks.iter().map(|s| json!({
            "rank": s.rank,
            "code": s.ts_code,
            "name": s.name,
            "pct_chg": s.pct_chg,
            "turnover_rate": s.turnover_rate,
            "circ_mv": s.circ_mv,
            "score": s.score,
        })).collect::<Vec<_>>()
    }))
}

/// Dispatch stock tool call by name.
pub async fn dispatch_stock_tool(name: &str, args: &Value) -> Result<Value> {
    match name {
        "stock_quote" => handle_stock_quote(args).await,
        "stock_select" => handle_stock_select(args).await,
        _ => bail!("Unknown stock tool: {}", name),
    }
}