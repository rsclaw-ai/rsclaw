use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::gateway_http::{self, GatewayEndpoint};
use super::style::*;
use crate::{agent, cli::MemoryCommand, config, sys::detect_memory_tier};

/// `rsclaw memory ...` dispatcher.
///
/// All read paths (Status, Search) probe the running gateway first and
/// route via HTTP so they see the same live view the agent runtime sees.
/// Direct redb is kept as a fallback only when the gateway is down — that
/// avoids the lock-conflict and stale-view problems documented in the
/// `reference_memory_readonly_lock` auto-memory note. Write paths (Index,
/// Save) are HTTP-only: redb's exclusive write lock means a CLI write can
/// only succeed by going through the gateway.
pub async fn cmd_memory(sub: MemoryCommand) -> Result<()> {
    let gateway_up = gateway_http::is_gateway_up().await;
    match sub {
        MemoryCommand::Status(args) => {
            if gateway_up {
                memory_status_http(args.json).await
            } else {
                memory_status_local(args.json).await
            }
        }
        MemoryCommand::Search(args) => {
            if gateway_up {
                memory_search_http(&args.query, args.max_results, args.json).await
            } else {
                memory_search_local(&args.query, args.max_results, args.json).await
            }
        }
        MemoryCommand::Save(args) => {
            if !gateway_up {
                anyhow::bail!(gateway_http::down_hint());
            }
            memory_save_http(args).await
        }
        MemoryCommand::Index(_args) => {
            // Re-index is a heavy write — leave the redb-direct path in
            // place for the "gateway stopped, fix the index" recovery
            // workflow. When the gateway is up there is no safe way for
            // the CLI to grab the write lock; bail with a hint.
            if gateway_up {
                anyhow::bail!(
                    "memory index requires exclusive access; stop the gateway first \
                     (`rsclaw gateway stop`) then re-run."
                );
            }
            memory_index_local().await
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP paths (preferred when gateway is up)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StatsResp {
    total: usize,
    #[serde(default)]
    pinned: usize,
    #[serde(default)]
    by_tier: std::collections::HashMap<String, usize>,
    #[serde(default)]
    by_kind: std::collections::HashMap<String, usize>,
}

async fn memory_status_http(json_out: bool) -> Result<()> {
    let resp: StatsResp = gateway_http::get_json("/api/v1/memory/stats").await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "documents": resp.total,
                "pinned": resp.pinned,
                "by_tier": resp.by_tier,
                "by_kind": resp.by_kind,
                "via": "http",
            }))?
        );
        return Ok(());
    }
    banner(&format!(
        "rsclaw memory v{} (via http)",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));
    kv("documents", &bold(&resp.total.to_string()));
    kv("pinned", &bold(&resp.pinned.to_string()));
    if !resp.by_tier.is_empty() {
        let mut parts: Vec<String> = resp
            .by_tier
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        parts.sort();
        kv("by_tier", &dim(&parts.join(" ")));
    }
    Ok(())
}

#[derive(Deserialize)]
struct ListDocsResp {
    docs: Vec<DocOut>,
    #[serde(default)]
    total: usize,
}

#[derive(Deserialize)]
struct DocOut {
    id: String,
    kind: String,
    text: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    tier: String,
    #[serde(default)]
    importance: f32,
    #[serde(default)]
    pinned: bool,
}

async fn memory_search_http(query: &str, max: usize, json_out: bool) -> Result<()> {
    let ep = GatewayEndpoint::resolve();
    let path = format!(
        "/api/v1/memory/docs?q={}&limit={}",
        urlencoding::encode(query),
        max
    );
    let resp: ListDocsResp = gateway_http::get_json(&path).await?;
    let _ = ep; // currently unused; reserved for future scope filters
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "query": query,
                "via": "http",
                "total": resp.total,
                "results": resp.docs.iter().map(|d| json!({
                    "id": d.id,
                    "scope": d.scope,
                    "kind": d.kind,
                    "tier": d.tier,
                    "importance": d.importance,
                    "pinned": d.pinned,
                    "text": d.text,
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }
    banner(&format!(
        "rsclaw memory search v{} (via http)",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));
    kv("query", &cyan(query));
    kv("results", &bold(&resp.docs.len().to_string()));
    if resp.docs.is_empty() {
        println!();
        warn_msg("no results");
        return Ok(());
    }
    println!();
    for d in &resp.docs {
        let pin_flag = if d.pinned { " 📌" } else { "" };
        println!(
            "  {} {} {}{}",
            dim(&format!("[{}]", &d.id[..8.min(d.id.len())])),
            dim(&format!("({}/{})", d.kind, d.tier)),
            d.text,
            pin_flag,
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct AddResp {
    id: String,
    scope: String,
    kind: String,
    tier: String,
    #[serde(default)]
    deduped: bool,
}

async fn memory_save_http(args: crate::cli::MemorySaveArgs) -> Result<()> {
    let body = json!({
        "text": args.text,
        "scope": args.scope,
        "kind": args.kind,
        "importance": args.importance,
        "pinned": args.pinned,
        "tags": args.tags,
    });
    let resp: AddResp = gateway_http::post_json("/api/v1/memory/docs", &body).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&Value::from(json!({
            "id": resp.id,
            "scope": resp.scope,
            "kind": resp.kind,
            "tier": resp.tier,
            "deduped": resp.deduped,
        })))?);
        return Ok(());
    }
    let prefix = if resp.deduped { "deduped" } else { "saved" };
    ok(&format!(
        "{prefix}: {} ({}/{}/{})",
        bold(&resp.id),
        resp.scope,
        resp.kind,
        resp.tier
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Local fallback paths — used only when the gateway is down.
//
// `open_readonly` opens its own redb handle in read-only mode. The auto-
// memory note `reference_memory_readonly_lock` records that this still
// races against a writing gateway in practice; we keep the path so the
// CLI is usable when the gateway is stopped, not as a primary route.
// ---------------------------------------------------------------------------

fn local_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let base = config::loader::base_dir();
    let data_dir = base.join("var/data");
    let model_dir = {
        let zh = base.join("models/bge-small-zh");
        let en = base.join("models/bge-small-en");
        if zh.join("config.json").exists() {
            zh
        } else {
            en
        }
    };
    (data_dir, model_dir)
}

async fn memory_status_local(json_out: bool) -> Result<()> {
    let (data_dir, model_dir) = local_paths();
    let cfg = config::load().ok();
    let search_cfg = cfg.as_ref().and_then(|c| c.raw.memory_search.as_ref());
    let mem =
        agent::memory::MemoryStore::open_readonly(&data_dir, Some(&model_dir), search_cfg).await?;
    let count = mem.count().await?;
    if json_out {
        println!(
            "{}",
            serde_json::json!({"documents": count, "via": "local"})
        );
    } else {
        banner(&format!(
            "rsclaw memory v{} (local readonly)",
            option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
        ));
        kv("documents", &bold(&count.to_string()));
        println!("{}", dim("gateway is down — counts may lag the live store"));
    }
    Ok(())
}

async fn memory_search_local(query: &str, max: usize, json_out: bool) -> Result<()> {
    let (data_dir, model_dir) = local_paths();
    let cfg = config::load().ok();
    let search_cfg = cfg.as_ref().and_then(|c| c.raw.memory_search.as_ref());
    let mut mem =
        agent::memory::MemoryStore::open_readonly(&data_dir, Some(&model_dir), search_cfg).await?;
    let results = mem.search_hybrid(query, None, max).await?;
    if json_out {
        println!(
            "{}",
            serde_json::json!({
                "query": query,
                "via": "local",
                "results": results.iter().map(|d| serde_json::json!({
                    "id": d.id,
                    "scope": d.scope,
                    "kind": d.kind,
                    "text": d.text,
                })).collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }
    banner(&format!(
        "rsclaw memory search v{} (local readonly)",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));
    if results.is_empty() {
        warn_msg("no results");
    } else {
        kv("query", &cyan(query));
        kv("results", &bold(&results.len().to_string()));
        println!();
        for doc in &results {
            println!(
                "  {} {} {}",
                dim(&format!("[{}]", doc.id)),
                dim(&format!("({})", doc.kind)),
                doc.text
            );
        }
    }
    println!("{}", dim("gateway is down — results may lag the live store"));
    Ok(())
}

async fn memory_index_local() -> Result<()> {
    let (data_dir, model_dir) = local_paths();
    let tier = detect_memory_tier();
    let cfg = config::load().ok();
    let search_cfg = cfg.as_ref().and_then(|c| c.raw.memory_search.as_ref());
    let mut mem =
        agent::memory::MemoryStore::open(&data_dir, Some(&model_dir), tier, search_cfg).await?;
    let count = mem.reindex().await?;
    ok(&format!(
        "re-indexed {} document(s)",
        bold(&count.to_string())
    ));
    Ok(())
}
