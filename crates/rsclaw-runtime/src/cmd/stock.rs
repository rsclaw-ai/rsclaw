//! `rsclaw stock <subcmd>` CLI handler.
//!
//! Routes the user's stock subcommand through the astock client.
//! Unlike the `memory` / `kb` CLI paths, there is no "local
//! fallback" here — A-share data lives behind astock's pytdx
//! connection, and re-opening that from a one-shot CLI process is
//! infeasible. When astock isn't configured we exit with a clean
//! hint pointing the user at the `astock` config block.

use anyhow::{Context, Result, bail};

use rsclaw_astock as astock;
use rsclaw_cli::{
    StockAskArgs, StockCommand, StockKlineArgs, StockQueryArgs, StockQuoteArgs,
    StockQuoteBatchArgs, StockSnapshotArgs,
};
use crate::cmd::gateway_http;

pub async fn cmd_stock(sub: StockCommand) -> Result<()> {
    // CLI mode runs out-of-process from the gateway; the global astock
    // client only lives inside `gateway run`. So either we connect via
    // the gateway HTTP loopback (and let the gateway's astock client
    // serve us) or we build a transient direct astock client from the
    // same config block.
    //
    // For consistency with `rsclaw memory` / `rsclaw kb` we prefer the
    // gateway-routing path: it shares connection pools / caches /
    // credits with the agent runtime, and admin tools that change
    // astock config at runtime take effect immediately. CLI-direct
    // is the fallback when gateway is down — the user can still pull
    // a quote from their terminal without starting the daemon.
    let gateway_up = gateway_http::is_gateway_up().await;
    if gateway_up {
        return run_via_gateway(sub).await;
    }
    run_direct(sub).await
}

// ---------------------------------------------------------------------------
// Direct path — instantiate AstockClient from config.
// ---------------------------------------------------------------------------

async fn run_direct(sub: StockCommand) -> Result<()> {
    let cfg = rsclaw_config::load().context("failed to load rsclaw config")?;
    let client = astock::AstockClient::from_config(cfg.raw.astock.as_ref())
        .map_err(|e| anyhow::anyhow!("{e}\nset config.astock.{{enabled, baseUrl}} in ~/.rsclaw/rsclaw.json5"))?;
    match sub {
        StockCommand::Quote(a) => do_quote(&client, a).await,
        StockCommand::QuoteBatch(a) => do_quote_batch(&client, a).await,
        StockCommand::Kline(a) => do_kline(&client, a).await,
        StockCommand::Snapshot(a) => do_snapshot(&client, a).await,
        StockCommand::Ask(a) => do_ask(&client, a).await,
        StockCommand::Query(a) => do_query(&client, a).await,
    }
}

async fn do_quote(client: &astock::AstockClient, a: StockQuoteArgs) -> Result<()> {
    let q = client
        .quote(&a.code)
        .await
        .map_err(|e| anyhow::anyhow!("astock quote: {e}"))?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&q)?);
    } else {
        print_quote_human(&q);
    }
    Ok(())
}

async fn do_quote_batch(client: &astock::AstockClient, a: StockQuoteBatchArgs) -> Result<()> {
    if a.codes.is_empty() {
        bail!("at least one --code required");
    }
    let qs = client
        .quote_batch(&a.codes)
        .await
        .map_err(|e| anyhow::anyhow!("astock quote_batch: {e}"))?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&qs)?);
    } else {
        for q in &qs {
            print_quote_human(q);
        }
    }
    Ok(())
}

async fn do_kline(client: &astock::AstockClient, a: StockKlineArgs) -> Result<()> {
    let r = client
        .kline(
            &a.code,
            Some(&a.period),
            Some(a.count),
            Some(a.offset),
            Some(&a.adjust),
        )
        .await
        .map_err(|e| anyhow::anyhow!("astock kline: {e}"))?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    println!(
        "{} {} {} bars (adjust={})",
        r.code,
        r.period,
        r.klines.len(),
        r.adjust
    );
    if let Some(w) = &r.adjust_warning {
        println!("  warning: {w}");
    }
    for k in r.klines.iter().rev().take(20).rev() {
        println!(
            "  ts={} O={:.2} H={:.2} L={:.2} C={:.2} vol={}",
            k.timestamp, k.open, k.high, k.low, k.close, k.volume
        );
    }
    Ok(())
}

async fn do_snapshot(client: &astock::AstockClient, a: StockSnapshotArgs) -> Result<()> {
    let rows = client
        .snapshot(
            a.ts.as_deref(),
            a.market.as_deref(),
            &a.codes,
            Some(&a.adjust),
        )
        .await
        .map_err(|e| anyhow::anyhow!("astock snapshot: {e}"))?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let total = rows.len();
    let take = if a.limit == 0 { total } else { a.limit.min(total) };
    println!("snapshot rows: {total} (showing {take})");
    for r in rows.iter().take(take) {
        let pct = if r.pre_close.abs() > f64::EPSILON {
            (r.price - r.pre_close) / r.pre_close * 100.0
        } else {
            0.0
        };
        println!(
            "  {} {} price={:.2} pct={:+.2}% amt={:.0}",
            r.code,
            r.name.as_deref().unwrap_or(""),
            r.price,
            pct,
            r.amount
        );
    }
    Ok(())
}

async fn do_ask(client: &astock::AstockClient, a: StockAskArgs) -> Result<()> {
    let resp = client
        .ask(&a.query, a.page, a.limit, a.call_type.as_deref())
        .await
        .map_err(|e| anyhow::anyhow!("astock ask: {e}"))?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        // Iwencai response shape varies; pretty-printing the raw JSON
        // is the most honest thing to do here. A schema-aware
        // formatter would routinely guess wrong.
        println!("{}", serde_json::to_string_pretty(&resp)?);
    }
    Ok(())
}

async fn do_query(client: &astock::AstockClient, a: StockQueryArgs) -> Result<()> {
    let r = client
        .query_sql(&a.sql)
        .await
        .map_err(|e| anyhow::anyhow!("astock query: {e}"))?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        println!("{}", r.columns.join(" | "));
        for row in &r.rows {
            let cells: Vec<String> = row
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            println!("{}", cells.join(" | "));
        }
    }
    Ok(())
}

fn print_quote_human(q: &astock::Quote) {
    let pct = q.change_pct();
    println!(
        "{} price={:.2} pct={:+.2}% O/H/L={:.2}/{:.2}/{:.2} pre={:.2} vol={} amt={:.0} ts={}",
        q.code, q.price, pct, q.open, q.high, q.low, q.pre_close, q.volume, q.amount, q.timestamp,
    );
}

// ---------------------------------------------------------------------------
// Gateway path — POST to gateway's stock endpoints (added in a later
// task). For now we forward to the direct path; once gateway exposes
// `/api/v1/stock/*` we'll switch this branch to call them so the
// caches/credits stay shared with the agent runtime.
// ---------------------------------------------------------------------------

async fn run_via_gateway(sub: StockCommand) -> Result<()> {
    // TODO: when `/api/v1/stock/*` exists, route through it so the CLI
    // shares the gateway's client + cache + paywall ledger. Until then,
    // fall back to building a direct client.
    run_direct(sub).await
}
