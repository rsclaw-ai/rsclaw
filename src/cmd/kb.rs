//! cmd_kb: dispatches the `rsclaw kb ...` subcommands.

use crate::cli::kb::KbCommand;
use crate::kb::compactor::run_compactor_tick;
use crate::kb::model::{CallerScope, KbStatus, KbVisibility};
use crate::kb::store::{docs, KbStore};
use crate::kb::sync::{KbSourceSyncer, ManualUploadSyncer, SyncContext, SyncReason, UrlSyncer};
use crate::kb::tools::{kb_fetch, kb_list_docs, kb_search};
use crate::kb::worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool};
use crate::kb::{KbEmbedder, KbIndex, KbPaths, StubEmbedder};
use crate::kb::search::SearchCtx;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

pub async fn cmd_kb(cmd: KbCommand, kb_root: PathBuf) -> Result<()> {
    match cmd {
        KbCommand::Add { path_or_url, tags } => add(kb_root, path_or_url, tags).await,
        KbCommand::Ls { tag, source_kind, limit } => ls(kb_root, tag, source_kind, limit),
        KbCommand::Rm { doc_id, yes } => rm(kb_root, doc_id, yes),
        KbCommand::Search { query, k } => search(kb_root, query, k),
        KbCommand::Show { id } => show(kb_root, id),
        KbCommand::Visibility { doc_id, visibility } => set_visibility(kb_root, doc_id, visibility),
        KbCommand::Compact => compact(kb_root),
        KbCommand::Stats => stats(kb_root),
    }
}

struct Handles {
    store: Arc<KbStore>,
    paths: Arc<KbPaths>,
    index: Arc<KbIndex>,
    embedder: Arc<dyn KbEmbedder>,
}

fn open_kb(kb_root: &PathBuf) -> Result<Handles> {
    let paths = Arc::new(KbPaths::new(kb_root));
    paths.ensure_layout().context("ensure_layout")?;
    let store = Arc::new(KbStore::open(&kb_root.join("kb.redb")).context("open kb.redb")?);
    let index = Arc::new(KbIndex::open_and_rebuild(&paths, &store).context("open index")?);
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    Ok(Handles { store, paths, index, embedder })
}

async fn add(kb_root: PathBuf, path_or_url: String, tags: Vec<String>) -> Result<()> {
    let h = open_kb(&kb_root)?;
    let ctx = SyncContext {
        store: h.store.clone(),
        paths: h.paths.clone(),
        index: h.index.clone(),
        embedder: h.embedder.clone(),
    };
    let outcome = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
        let syncer = UrlSyncer {
            url: path_or_url.clone(),
            tags,
        };
        syncer
            .sync(&ctx, SyncReason::Manual)
            .await
            .map_err(|e| anyhow::anyhow!("url sync failed: {e}"))?
    } else {
        let syncer = ManualUploadSyncer {
            source_id: format!("manual:{path_or_url}"),
            file_path: PathBuf::from(&path_or_url),
            tags,
        };
        syncer
            .sync(&ctx, SyncReason::Manual)
            .await
            .map_err(|e| anyhow::anyhow!("manual sync failed: {e}"))?
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| format!("{outcome:?}"))
    );

    // Drain pending ChunkAndEmbed jobs so a follow-up `kb search`
    // sees the new chunks. In production the gateway daemon's worker
    // pool handles this; for CLI-only mode we run a one-shot drain.
    let hctx = HandlerCtx {
        store: h.store.clone(),
        paths: h.paths.clone(),
        embedder: h.embedder.clone(),
        index: h.index.clone(),
    };
    let cfg = WorkerConfig::default();
    loop {
        let did = WorkerPool::run_one_blocking(&hctx, &cfg, &DefaultDispatcher)?;
        if !did {
            break;
        }
    }
    Ok(())
}

fn ls(
    kb_root: PathBuf,
    tag: Vec<String>,
    source_kind: Option<String>,
    limit: usize,
) -> Result<()> {
    let h = open_kb(&kb_root)?;
    let ctx = search_ctx(&h);
    let out = kb_list_docs::run(
        &ctx,
        kb_list_docs::KbListDocsInput {
            tags: tag,
            source_kind,
            limit,
            cursor: None,
        },
        &CallerScope::default(),
    )?;
    if out.docs.is_empty() {
        println!("(no documents)");
        return Ok(());
    }
    println!(
        "{:<26} {:<8} v {:<5}  tags                            title",
        "doc_id", "kind", ""
    );
    for d in &out.docs {
        println!(
            "{:<26} {:<8} v {:<5}  {:<30}  {}",
            d.doc_id,
            d.source_kind,
            d.version,
            d.tags.join(","),
            d.title
        );
    }
    if let Some(next) = out.next_cursor {
        println!("\n(cursor for next page: {next})");
    }
    Ok(())
}

fn rm(kb_root: PathBuf, doc_id: String, yes: bool) -> Result<()> {
    let h = open_kb(&kb_root)?;
    if !yes {
        eprintln!("Refusing to tombstone without --yes (this is a destructive operation).");
        return Ok(());
    }
    let rtx = h.store.begin_read()?;
    let mut d = docs::get(&rtx, &doc_id)?
        .ok_or_else(|| anyhow::anyhow!("doc not found: {doc_id}"))?;
    drop(rtx);
    d.status = KbStatus::Tombstoned;
    let wtx = h.store.begin_write()?;
    docs::put(&wtx, &d)?;
    wtx.commit()?;
    println!("tombstoned {doc_id}");
    Ok(())
}

fn search(kb_root: PathBuf, query: String, k: usize) -> Result<()> {
    let h = open_kb(&kb_root)?;
    let ctx = search_ctx(&h);
    let out = kb_search::run(
        &ctx,
        kb_search::KbSearchInput {
            query,
            k,
            filter: Default::default(),
            mode: "hybrid".into(),
            diversity: "mmr".into(),
            mmr_lambda: 0.5,
            boost_entities: vec![],
        },
        &CallerScope::default(),
    )?;
    if out.results.is_empty() {
        println!("(no hits)");
        return Ok(());
    }
    for (i, hit) in out.results.iter().enumerate() {
        println!(
            "[{}] {:.3}  {}  {}",
            i + 1,
            hit.score,
            hit.doc_title,
            hit.citation.locator_human
        );
        let snippet: String = hit.text.chars().take(160).collect();
        println!("    {snippet}");
        println!("    chunk_id={}", hit.chunk_id);
    }
    Ok(())
}

fn show(kb_root: PathBuf, id: String) -> Result<()> {
    let h = open_kb(&kb_root)?;
    let ctx = search_ctx(&h);

    // Resolve id: try chunk_id first (32 hex chars from Week 1's
    // deterministic chunker), else treat as doc_id and list its
    // chunks. Falls back to "not found" if neither resolves.
    let is_chunk_id = id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit());
    if is_chunk_id {
        return show_chunk(&ctx, id);
    }

    // Treat as doc_id: print doc metadata + every chunk's heading
    // path + first snippet (180 chars).
    let rtx = h.store.clone().begin_read()?;
    let doc = match crate::kb::store::docs::get(&rtx, &id)? {
        Some(d) => d,
        None => {
            // Last chance: maybe it really was a chunk_id with an
            // unconventional length. Try kb_fetch anyway.
            drop(rtx);
            return show_chunk(&ctx, id);
        }
    };
    if !doc.visible_to(&CallerScope::default()) {
        eprintln!("doc not visible to current scope");
        return Ok(());
    }
    println!("doc_id:    {}", doc.id);
    println!("title:     {}", doc.title);
    println!("source:    {:?}", doc.source);
    println!("kind:      {}", doc.source_kind.as_str());
    println!("version:   {}", doc.version);
    println!("status:    {:?}", doc.status);
    if !doc.tags.is_empty() {
        println!("tags:      {}", doc.tags.join(", "));
    }
    let chunks_list =
        crate::kb::store::chunks::chunks_for_logical(&rtx, &doc.logical_source_id)?;
    let mut chunks_this_version: Vec<_> = chunks_list
        .into_iter()
        .filter(|c| c.doc_id == doc.id)
        .collect();
    chunks_this_version.sort_by_key(|c| c.seq);
    println!("chunks:    {}", chunks_this_version.len());
    println!("---");
    for c in &chunks_this_version {
        let head = if c.heading_path.is_empty() {
            String::from("(root)")
        } else {
            c.heading_path.join(" > ")
        };
        let snippet: String = c.indexed_text.chars().take(180).collect();
        println!("[{}]  §{}", c.id, head);
        println!("    {snippet}");
    }
    Ok(())
}

fn show_chunk(ctx: &SearchCtx, id: String) -> Result<()> {
    let out = kb_fetch::run(
        ctx,
        kb_fetch::KbFetchInput {
            chunk_id: id,
            expand: "neighbor".into(),
        },
        &CallerScope::default(),
    )?;
    match out {
        Some(o) => {
            println!("doc_id: {}", o.chunk.doc_id);
            println!("heading: {}", o.chunk.heading_path.join(" > "));
            println!("---");
            println!("{}", o.chunk.text);
            if !o.neighbors.is_empty() {
                println!("\n--- neighbors ---");
                for n in o.neighbors {
                    println!("[{}]\n{}\n", n.chunk_id, n.text);
                }
            }
        }
        None => {
            eprintln!("not found or not visible to current scope");
        }
    }
    Ok(())
}

fn set_visibility(kb_root: PathBuf, doc_id: String, visibility: String) -> Result<()> {
    let h = open_kb(&kb_root)?;
    let new_vis = parse_visibility(&visibility)
        .ok_or_else(|| anyhow::anyhow!("invalid visibility: {visibility}"))?;
    let rtx = h.store.begin_read()?;
    let mut d = docs::get(&rtx, &doc_id)?
        .ok_or_else(|| anyhow::anyhow!("doc not found: {doc_id}"))?;
    drop(rtx);
    d.visibility = new_vis;
    let wtx = h.store.begin_write()?;
    docs::put(&wtx, &d)?;
    wtx.commit()?;
    println!("updated {doc_id} visibility → {visibility}");
    Ok(())
}

fn parse_visibility(s: &str) -> Option<KbVisibility> {
    match s {
        "global" => Some(KbVisibility::Global),
        "private" => Some(KbVisibility::Private),
        _ => {
            if let Some(id) = s.strip_prefix("agent:") {
                Some(KbVisibility::Agent { agent_id: id.to_string() })
            } else if let Some(id) = s.strip_prefix("channel:") {
                Some(KbVisibility::Channel { channel_id: id.to_string() })
            } else {
                None
            }
        }
    }
}

fn compact(kb_root: PathBuf) -> Result<()> {
    let h = open_kb(&kb_root)?;
    let now = chrono::Utc::now().timestamp_millis();
    let stats = run_compactor_tick(&h.store, &h.paths, now)?;
    println!(
        "{}",
        serde_json::json!({
            "orphans_deleted": stats.orphans_deleted,
            "ledger_advanced_to_cleanup": stats.ledger_advanced_to_cleanup,
            "ledger_advanced_to_done": stats.ledger_advanced_to_done,
        })
    );
    Ok(())
}

fn stats(kb_root: PathBuf) -> Result<()> {
    use redb::ReadableTable;
    let h = open_kb(&kb_root)?;
    let rtx = h.store.begin_read()?;
    let mut counts = serde_json::Map::new();
    for (name, td) in [
        ("kb_docs", crate::kb::store::schema::KB_DOCS),
        ("kb_chunks", crate::kb::store::schema::KB_CHUNKS),
        ("kb_ledger", crate::kb::store::schema::KB_LEDGER),
        ("kb_jobs_by_id", crate::kb::store::schema::KB_JOBS_BY_ID),
        ("kb_seen_items", crate::kb::store::schema::KB_SEEN_ITEMS),
    ] {
        let tbl = rtx.open_table(td)?;
        let n = tbl.iter()?.count();
        counts.insert(name.into(), serde_json::Value::Number(n.into()));
    }
    println!(
        "{}",
        serde_json::Value::Object(counts)
    );
    Ok(())
}

fn search_ctx(h: &Handles) -> SearchCtx {
    SearchCtx {
        store: h.store.clone(),
        index: h.index.clone(),
        paths: h.paths.clone(),
        embedder: h.embedder.clone(),
    }
}
