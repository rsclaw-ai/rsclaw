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
        KbCommand::Add { path_or_url, tags, recursive, ext } => {
            add(kb_root, path_or_url, tags, recursive, ext).await
        }
        KbCommand::Ls { tag, source_kind, limit } => ls(kb_root, tag, source_kind, limit),
        KbCommand::Rm { doc_id, tag, yes } => rm(kb_root, doc_id, tag, yes),
        KbCommand::Search { query, k } => search(kb_root, query, k),
        KbCommand::Show { id } => show(kb_root, id),
        KbCommand::Visibility { doc_id, visibility } => set_visibility(kb_root, doc_id, visibility),
        KbCommand::Compact => compact(kb_root),
        KbCommand::Stats => stats(kb_root),
        KbCommand::Export { doc_id, to } => export(kb_root, doc_id, to),
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

async fn add(
    kb_root: PathBuf,
    path_or_url: String,
    tags: Vec<String>,
    recursive: bool,
    ext: String,
) -> Result<()> {
    let h = open_kb(&kb_root)?;
    let ctx = SyncContext {
        store: h.store.clone(),
        paths: h.paths.clone(),
        index: h.index.clone(),
        embedder: h.embedder.clone(),
    };

    let is_url =
        path_or_url.starts_with("http://") || path_or_url.starts_with("https://");
    if is_url {
        let syncer = UrlSyncer {
            url: path_or_url.clone(),
            tags,
        };
        let outcome = syncer
            .sync(&ctx, SyncReason::Manual)
            .await
            .map_err(|e| anyhow::anyhow!("url sync failed: {e}"))?;
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| format!("{outcome:?}"))
        );
        drain_worker(&h)?;
        return Ok(());
    }

    let path = PathBuf::from(&path_or_url);
    let files = if recursive && path.is_dir() {
        collect_files(&path, &ext)
    } else {
        vec![path.clone()]
    };

    let mut total = serde_json::Map::new();
    total.insert("added".into(), serde_json::Value::Number(0u64.into()));
    total.insert("skipped".into(), serde_json::Value::Number(0u64.into()));
    let mut added = 0u64;
    let mut skipped = 0u64;
    for f in &files {
        let syncer = ManualUploadSyncer {
            source_id: format!("manual:{}", f.display()),
            file_path: f.clone(),
            tags: tags.clone(),
        };
        match syncer.sync(&ctx, SyncReason::Manual).await {
            Ok(outcome) => {
                added += outcome.docs_added as u64;
                skipped += outcome.docs_skipped as u64;
            }
            Err(e) => {
                eprintln!("skip {}: {e}", f.display());
                skipped += 1;
            }
        }
    }
    drain_worker(&h)?;
    total.insert("added".into(), serde_json::Value::Number(added.into()));
    total.insert("skipped".into(), serde_json::Value::Number(skipped.into()));
    total.insert(
        "files_seen".into(),
        serde_json::Value::Number((files.len() as u64).into()),
    );
    println!("{}", serde_json::Value::Object(total));
    Ok(())
}

fn collect_files(root: &PathBuf, ext_csv: &str) -> Vec<PathBuf> {
    let allowed: Vec<String> = ext_csv
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.clone()];
    while let Some(p) = stack.pop() {
        let rd = match std::fs::read_dir(&p) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() {
                    let ext = path
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if allowed.iter().any(|a| a == &ext) {
                        out.push(path);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn drain_worker(h: &Handles) -> Result<()> {
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

fn rm(
    kb_root: PathBuf,
    doc_id: Option<String>,
    tag: Option<String>,
    yes: bool,
) -> Result<()> {
    let h = open_kb(&kb_root)?;
    if !yes {
        eprintln!("Refusing to tombstone without --yes (this is a destructive operation).");
        return Ok(());
    }
    match (doc_id, tag) {
        (Some(id), None) => rm_by_id(&h, id),
        (None, Some(tag)) => rm_by_tag(&h, &tag),
        (Some(_), Some(_)) => {
            anyhow::bail!("pass either doc_id or --tag, not both")
        }
        (None, None) => anyhow::bail!("pass either a doc_id or --tag <name>"),
    }
}

fn rm_by_id(h: &Handles, doc_id: String) -> Result<()> {
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

fn rm_by_tag(h: &Handles, tag: &str) -> Result<()> {
    use crate::kb::store::codec::decode;
    use redb::ReadableTable;
    let rtx = h.store.begin_read()?;
    let mut to_tombstone: Vec<crate::kb::model::KbDoc> = Vec::new();
    {
        let tbl = rtx.open_table(crate::kb::store::schema::KB_DOCS)?;
        for entry in tbl.iter()? {
            let (_, v) = entry?;
            let d: crate::kb::model::KbDoc = decode(v.value())?;
            if d.status == KbStatus::Active && d.tags.iter().any(|t| t == tag) {
                to_tombstone.push(d);
            }
        }
    }
    drop(rtx);
    if to_tombstone.is_empty() {
        println!("no Active docs with tag={tag}");
        return Ok(());
    }
    let wtx = h.store.begin_write()?;
    for mut d in to_tombstone.iter().cloned() {
        d.status = KbStatus::Tombstoned;
        docs::put(&wtx, &d)?;
    }
    wtx.commit()?;
    println!("tombstoned {} docs with tag={tag}", to_tombstone.len());
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
    // Spec §6 pairs the compactor tick with an HNSW snapshot dump.
    // Errors here are non-fatal — snapshot is a performance
    // optimisation; the next startup just rebuilds from redb.
    let snapshot_status = match h.index.snapshot_hnsw(&h.paths) {
        Ok(()) => "ok",
        Err(e) => {
            tracing::warn!("kb snapshot dump failed: {e:#}");
            "failed"
        }
    };
    println!(
        "{}",
        serde_json::json!({
            "orphans_deleted": stats.orphans_deleted,
            "ledger_advanced_to_cleanup": stats.ledger_advanced_to_cleanup,
            "ledger_advanced_to_done": stats.ledger_advanced_to_done,
            "hnsw_snapshot": snapshot_status,
        })
    );
    Ok(())
}

fn stats(kb_root: PathBuf) -> Result<()> {
    use crate::kb::store::codec::decode;
    use redb::ReadableTable;
    let h = open_kb(&kb_root)?;
    let rtx = h.store.begin_read()?;
    let mut counts = serde_json::Map::new();
    // Per-status doc breakdown.
    let docs_tbl = rtx.open_table(crate::kb::store::schema::KB_DOCS)?;
    let mut active = 0u64;
    let mut tombstoned = 0u64;
    for entry in docs_tbl.iter()? {
        let (_, v) = entry?;
        let d: crate::kb::model::KbDoc = decode(v.value())?;
        match d.status {
            crate::kb::model::KbStatus::Active => active += 1,
            crate::kb::model::KbStatus::Tombstoned => tombstoned += 1,
            _ => {}
        }
    }
    counts.insert("docs_active".into(), serde_json::Value::Number(active.into()));
    counts.insert(
        "docs_tombstoned".into(),
        serde_json::Value::Number(tombstoned.into()),
    );
    // Other table sizes.
    for (name, td) in [
        ("kb_chunks", crate::kb::store::schema::KB_CHUNKS),
        ("kb_ledger", crate::kb::store::schema::KB_LEDGER),
        ("kb_jobs_by_id", crate::kb::store::schema::KB_JOBS_BY_ID),
        ("kb_seen_items", crate::kb::store::schema::KB_SEEN_ITEMS),
        ("kb_entities", crate::kb::store::schema::KB_ENTITIES),
        (
            "kb_entity_index",
            crate::kb::store::schema::KB_ENTITY_INDEX,
        ),
    ] {
        let tbl = rtx.open_table(td)?;
        let n = tbl.iter()?.count();
        counts.insert(name.into(), serde_json::Value::Number(n.into()));
    }
    // Disk usage: sum of md/ + raw/ + kb.redb + idx/ tree.
    let disk_bytes = total_size(&kb_root);
    counts.insert(
        "disk_bytes".into(),
        serde_json::Value::Number(disk_bytes.into()),
    );
    println!("{}", serde_json::Value::Object(counts));
    Ok(())
}

fn total_size(root: &PathBuf) -> u64 {
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = vec![root.clone()];
    while let Some(p) = stack.pop() {
        let read = match std::fs::read_dir(&p) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() {
                    if let Ok(meta) = path.metadata() {
                        total += meta.len();
                    }
                }
            }
        }
    }
    total
}

fn export(kb_root: PathBuf, doc_id: String, to: PathBuf) -> Result<()> {
    let h = open_kb(&kb_root)?;
    let rtx = h.store.begin_read()?;
    let doc = docs::get(&rtx, &doc_id)?
        .ok_or_else(|| anyhow::anyhow!("doc not found: {doc_id}"))?;
    if !doc.visible_to(&CallerScope::default()) {
        anyhow::bail!("doc not visible to current scope");
    }
    let abs = h.paths.root.join(&doc.markdown_path);
    let body = crate::kb::content_store::read::read_doc_body(&abs)
        .with_context(|| format!("read {}", abs.display()))?;
    if let Some(parent) = to.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
        }
    }
    std::fs::write(&to, &body).with_context(|| format!("write {}", to.display()))?;
    println!("wrote {} ({} bytes) → {}", doc.title, body.len(), to.display());
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
