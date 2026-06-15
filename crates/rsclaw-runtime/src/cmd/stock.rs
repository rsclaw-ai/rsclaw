//! `rsclaw stock <subcmd>` CLI handler.
//!
//! Stock data is now provided by the commercial astock WASM plugin. The
//! one-shot CLI no longer links the private astock client into the open core.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use rsclaw_cli::{StockCommand, StockSnapshotArgs};

/// Handle `rsclaw stock ...`.
pub async fn cmd_stock(sub: StockCommand) -> Result<()> {
    let plugin = load_astock_plugin().await?;
    let (tool, args, json_output) = match sub {
        StockCommand::Quote(a) => ("quote", json!({ "code": a.code }), a.json),
        StockCommand::QuoteBatch(a) => {
            if a.codes.is_empty() {
                bail!("at least one --code required");
            }
            ("quote", json!({ "codes": a.codes }), a.json)
        }
        StockCommand::Kline(a) => (
            "kline",
            json!({
                "code": a.code,
                "period": a.period,
                "count": a.count,
                "offset": a.offset,
                "adjust": a.adjust,
            }),
            a.json,
        ),
        StockCommand::Snapshot(a) => ("snapshot", snapshot_args(&a), a.json),
        StockCommand::Ask(a) => (
            "ask",
            json!({
                "query": a.query,
                "page": a.page,
                "limit": a.limit,
                "call_type": a.call_type,
            }),
            a.json,
        ),
        StockCommand::Query(a) => ("query", json!({ "sql": a.sql }), a.json),
    };
    let out = plugin.call_tool(tool, args).await?;
    print_plugin_result(&out, json_output)
}

fn snapshot_args(a: &StockSnapshotArgs) -> Value {
    json!({
        "ts": a.ts.as_deref(),
        "market": a.market.as_deref(),
        "codes": &a.codes,
        "adjust": a.adjust.as_str(),
        "limit": a.limit,
        "use_watchlist": false,
    })
}

async fn load_astock_plugin() -> Result<rsclaw_plugin::WasmPlugin> {
    let plugin_dir = astock_plugin_dir()?;
    let manifest = rsclaw_plugin::load_manifest(&plugin_dir)
        .with_context(|| format!("failed to load astock plugin manifest: {}", plugin_dir.display()))?;
    if !manifest.is_wasm() {
        bail!("astock plugin must use wasm runtime: {}", plugin_dir.display());
    }
    let mut cfg = wasmtime::Config::new();
    cfg.async_support(true);
    let engine = wasmtime::Engine::new(&cfg).context("create astock wasm engine")?;
    let browser = Arc::new(Mutex::new(None));
    rsclaw_plugin::load_wasm_plugin(&manifest, &engine, browser, None, None)
        .await
        .with_context(|| format!("failed to load astock WASM plugin: {}", plugin_dir.display()))
}

fn astock_plugin_dir() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("RSCLAW_ASTOCK_WASM_PLUGIN_DIR") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(home) = dirs_next::home_dir() {
        candidates.push(home.join("dev/rsclaw-plugins/astock"));
        candidates.push(home.join(".rsclaw/plugins/astock"));
    }
    for path in candidates {
        if path.join("plugin.json5").exists() && path.join("astock.wasm").exists() {
            return Ok(path);
        }
    }
    bail!(
        "astock WASM plugin not found. Set RSCLAW_ASTOCK_WASM_PLUGIN_DIR or install it at ~/dev/rsclaw-plugins/astock"
    )
}

fn print_plugin_result(out: &Value, json_output: bool) -> Result<()> {
    let ok = out.get("ok").and_then(Value::as_bool).unwrap_or(true);
    if !ok && !json_output {
        bail!(
            "{}",
            out.get("error")
                .and_then(Value::as_str)
                .unwrap_or("astock plugin returned ok=false")
        );
    }
    if json_output {
        let value = if ok {
            out.get("data").unwrap_or(out)
        } else {
            out
        };
        println!("{}", serde_json::to_string_pretty(value)?);
        return Ok(());
    }
    if let Some(markdown) = out.get("markdown").and_then(Value::as_str) {
        println!("{markdown}");
    } else if let Some(data) = out.get("data") {
        println!("{}", serde_json::to_string_pretty(data)?);
    } else {
        println!("{}", serde_json::to_string_pretty(out)?);
    }
    Ok(())
}
