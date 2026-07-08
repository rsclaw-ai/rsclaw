//! Stock market tools for rsclaw agent.
//!
//! Integrates astock-core functionality into rsclaw's tool system:
//! - `stock_quote`: Real-time stock quotes via HTTP (Eastmoney/Tencent/Sina)
//! - `stock_select`: Post-market stock selection via Tushare (requires token)
//! - `stock_lhb`: Dragon-Tiger list analysis (龙虎榜) via CDP
//! - `stock_debate`: Bull/Bear debate analysis via LLM
//! - `stock_iwencai`: Natural language stock query via iWenCai (问财)

use std::sync::Arc;
use anyhow::{Result, Context, bail};
use serde_json::{Value, json};

use super::stock_capability::{RsclawCdp, RsclawConfig, create_http_client, StockLlmContext};
use astock_core::StockEngine;
use astock_core::capability::ConfigProvider;
use astock_core::algo::selector::SelectionStrategy;
use rsclaw_provider::{registry::ProviderRegistry, failover::FailoverManager};

// ============================================================================
// Tool Definitions
// ============================================================================

/// Tool definitions for stock tools.
pub fn stock_tool_defs() -> Vec<rsclaw_provider::ToolDef> {
    vec![
        rsclaw_provider::ToolDef {
            name: "stock_quote".to_owned(),
            description: "获取实时股票行情. \
                数据源: 东方财富 → 腾讯 → 新浪 HTTP (自动切换). \
                输入: 股票代码数组, 如 ['600519.SH', '000001.SZ']. \
                返回: 当前价、涨跌幅、成交量、换手率、市值等. \
                无需 tushare_token.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "codes": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "股票代码 (ts_code 格式), 如 ['600519.SH', '000001.SZ']"
                    }
                },
                "required": ["codes"]
            }),
        },
        rsclaw_provider::ToolDef {
            name: "stock_select".to_owned(),
            description: "量化选股 - 根据条件筛选股票. \
                条件: 换手率、市值、涨幅等. \
                数据源: Tushare (需要在 rsclaw.json5 配置 tushare_token). \
                用途: '选股', '筛选股票', '找出符合条件的股票'.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "trade_date": {
                        "type": "string",
                        "description": "交易日期 (YYYY-MM-DD). 默认: 最近交易日."
                    },
                    "turnover_rate_min": {
                        "type": "number",
                        "description": "最小换手率 (%). 默认: 3."
                    },
                    "market_cap_max_yi": {
                        "type": "number",
                        "description": "最大流通市值 (亿元). 默认: 100."
                    },
                    "change_pct_min": {
                        "type": "number",
                        "description": "最小涨幅 (%). 默认: 0."
                    },
                    "change_pct_max": {
                        "type": "number",
                        "description": "最大涨幅 (%). 默认: 10 (排除极端)."
                    },
                    "select_count": {
                        "type": "integer",
                        "description": "返回数量上限. 默认: 20."
                    }
                }
            }),
        },
        rsclaw_provider::ToolDef {
            name: "stock_lhb".to_owned(),
            description: "龙虎榜分析 - 查询每日龙虎榜数据. \
                数据源: 东方财富网站 (CDP 抓取). \
                返回: 上榜股票、净买额、涨幅、上榜原因等. \
                用途: '龙虎榜', '今天龙虎榜', '游资动向'.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "date": {
                        "type": "string",
                        "description": "查询日期 (YYYY-MM-DD). 默认: 今天."
                    }
                },
                "required": []
            }),
        },
        rsclaw_provider::ToolDef {
            name: "stock_debate".to_owned(),
            description: "多空辩论 - 对股票进行多空分析. \
                通过 LLM 生成多头观点(上涨理由)和空头观点(下跌风险), 再综合判断. \
                用途: '分析茅台', '茅台多空', '600519 看法'.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "股票代码 (ts_code 格式), 如 '600519.SH'"
                    },
                    "quick": {
                        "type": "boolean",
                        "description": "快速模式 (单次 LLM 调用). 默认: false."
                    }
                },
                "required": ["code"]
            }),
        },
        rsclaw_provider::ToolDef {
            name: "stock_iwencai".to_owned(),
            description: "问财查询 - 用自然语言选股. \
                数据源: 同花顺问财 (iWenCai). \
                示例: '今天涨停的股票', '市值小于50亿的半导体'.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "自然语言查询, 如 '今天涨停的股票'"
                    }
                },
                "required": ["question"]
            }),
        },
    ]
}

// ============================================================================
// StockToolContext - holds runtime references for stock tools
// ============================================================================

/// Context passed from runtime to stock tool handlers.
/// Contains references to LLM providers and configuration.
pub struct StockToolContext {
    pub providers: Arc<ProviderRegistry>,
    pub failover: FailoverManager,
}

impl StockToolContext {
    pub fn new(
        providers: Arc<ProviderRegistry>,
        failover: FailoverManager,
    ) -> Self {
        Self { providers, failover }
    }
}

impl std::fmt::Debug for StockToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StockToolContext")
            .field("providers", &self.providers.names())
            .finish()
    }
}

// ============================================================================
// Tool Handlers
// ============================================================================

/// Build a StockEngine with available capabilities.
/// For tools that don't need LLM (quote, select, lhb, iwencai).
fn build_engine_basic() -> Result<StockEngine> {
    let http = create_http_client()
        .context("Failed to create HTTP client for stock engine")?;

    let config = RsclawConfig::new();

    StockEngine::builder()
        .http(http)
        .cdp(RsclawCdp::new())
        .llm(astock_core::capability::NoLlm)
        .config(config)
        .build()
        .context("Failed to build StockEngine")
}

/// Build a StockEngine with LLM capability.
/// For tools that need LLM (debate).
fn build_engine_with_llm(ctx: &StockToolContext) -> Result<StockEngine> {
    let http = create_http_client()
        .context("Failed to create HTTP client for stock engine")?;

    let config = RsclawConfig::new();

    // Get model from config, or use default
    let model = config.llm_model.clone()
        .unwrap_or_else(|| {
            // Fallback: try to find a default model from providers
            // Most users have at least one provider configured
            "deepseek/deepseek-chat".to_owned()
        });

    let llm_ctx = StockLlmContext::new(
        Arc::clone(&ctx.providers),
        ctx.failover.clone(),
        model,
    );

    // Create RsclawLlm directly, not boxed
    let llm = crate::stock_capability::RsclawLlm(llm_ctx);

    StockEngine::builder()
        .http(http)
        .cdp(RsclawCdp::new())
        .llm(llm)  // Pass RsclawLlm directly, it implements LlmCapability
        .config(config)
        .build()
        .context("Failed to build StockEngine with LLM")
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

    let engine = build_engine_basic()?;

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

    let engine = build_engine_basic()?;

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

/// Handle stock_lhb (龙虎榜) tool call.
/// Uses CDP to scrape Eastmoney dragon-tiger list.
pub async fn handle_stock_lhb(args: &Value) -> Result<Value> {
    let engine = build_engine_basic()?;

    // Get date from args, default to today
    let date = args.get("date")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            chrono::Local::now().format("%Y-%m-%d").to_string()
        });

    let report = engine.lhb(&date).await?;

    // Return pre-rendered markdown plus structured data
    Ok(json!({
        "success": true,
        "source": "eastmoney CDP",
        "markdown": report.markdown,
        "date": report.trade_date,
        "title": report.title,
        // Extract stats from extra if available
        "stats": report.extra.as_ref().and_then(|e| e.get("stats")).cloned(),
    }))
}

/// Handle stock_debate (多空辩论) tool call.
/// Uses LLM to generate bull/bear analysis.
pub async fn handle_stock_debate(args: &Value, ctx: &StockToolContext) -> Result<Value> {
    let code = args["code"]
        .as_str()
        .context("code is required")?;

    let quick = args.get("quick")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let engine = build_engine_with_llm(ctx)?;

    if quick {
        // Quick debate: single LLM call
        let result = engine.quick_debate(code).await?;
        Ok(json!({
            "success": true,
            "mode": "quick",
            "code": code,
            "analysis": result,
        }))
    } else {
        // Full debate: 3 LLM calls (bull, bear, summary)
        let report = engine.debate(code).await?;
        Ok(json!({
            "success": true,
            "mode": "full",
            "markdown": report.markdown,
            "title": report.title,
            "date": report.trade_date,
            // Extract debate result from extra if available
            "debate": report.extra.as_ref().and_then(|e| e.get("debate")).cloned(),
        }))
    }
}

/// Handle stock_iwencai (问财) tool call.
/// Uses CDP to query iWenCai natural language search.
pub async fn handle_stock_iwencai(args: &Value) -> Result<Value> {
    let question = args["question"]
        .as_str()
        .context("question is required")?;

    let engine = build_engine_basic()?;

    let result = engine.iwencai(question).await?;

    Ok(json!({
        "success": true,
        "source": "iwencai CDP",
        "query": result.query,
        "summary": result.summary,
        "stocks": result.stocks.iter().map(|s| json!({
            "code": s.ts_code,
            "name": s.name,
            "price": s.price,
            "change_pct": s.change_pct,
        })).collect::<Vec<_>>(),
        "count": result.stocks.len(),
    }))
}

/// Dispatch stock tool call by name.
/// ctx is optional - only needed for tools that use LLM (debate).
pub async fn dispatch_stock_tool(name: &str, args: &Value, ctx: Option<&StockToolContext>) -> Result<Value> {
    match name {
        "stock_quote" => handle_stock_quote(args).await,
        "stock_select" => handle_stock_select(args).await,
        "stock_lhb" => handle_stock_lhb(args).await,
        "stock_debate" => {
            let ctx = ctx.context("stock_debate requires LLM context (providers + failover)")?;
            handle_stock_debate(args, ctx).await
        }
        "stock_iwencai" => handle_stock_iwencai(args).await,
        _ => bail!("Unknown stock tool: {}", name),
    }
}