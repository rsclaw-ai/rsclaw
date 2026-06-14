//! cmd_kb: dispatches the `rsclaw kb ...` subcommands.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};

use crate::cmd::style::{banner, bold, cyan, dim, kv, ok};
use rsclaw_cli::kb::KbCommand;
use rsclaw_kb::{
        KbEmbedder, KbIndex, KbPaths,
        compactor::run_compactor_tick,
        model::{CallerScope, KbStatus, KbVisibility},
        search::SearchCtx,
        store::{KbStore, docs},
        sync::{KbSourceSyncer, ManualUploadSyncer, SyncContext, SyncReason, UrlSyncer},
        tools::{kb_fetch, kb_list_docs, kb_search},
        worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool},
    };

pub async fn cmd_kb(cmd: KbCommand, kb_root: PathBuf) -> Result<()> {
    // HTTP-first routing for the read/write ops a running gateway
    // already serves. When the gateway is up the redb file is held
    // under its exclusive write lock, so a direct CLI open either
    // fights the lock or sees a stale view — the same footgun
    // documented for memory in `reference_memory_readonly_lock`.
    //
    // Only the ops that have a matching HTTP endpoint route through
    // here; the rest (Show / Rm / Visibility / Compact / Export /
    // SyncAll) fall through to their existing direct-store impls.
    let gateway_up = crate::cmd::gateway_http::is_gateway_up().await;
    if gateway_up {
        match &cmd {
            KbCommand::Search { query, k, json } => {
                return kb_search_http(query, *k, *json).await;
            }
            KbCommand::Stats => {
                return kb_stats_http().await;
            }
            KbCommand::Compact => {
                return kb_compact_http().await;
            }
            KbCommand::Ls {
                tag,
                source_kind,
                limit,
            } => {
                return kb_ls_http(tag, source_kind.as_deref(), *limit).await;
            }
            KbCommand::Rm { doc_id, tag, yes } => {
                return kb_rm_http(doc_id.clone(), tag.clone(), *yes).await;
            }
            KbCommand::Show { id } => {
                return kb_show_http(id).await;
            }
            KbCommand::Visibility { doc_id, visibility } => {
                return kb_visibility_http(doc_id, visibility).await;
            }
            KbCommand::Export { doc_id, to } => {
                return kb_export_http(doc_id, to).await;
            }
            KbCommand::SyncAll {
                interval_min,
                max,
                dry_run,
            } => {
                return kb_sync_all_http(*interval_min, *max, *dry_run).await;
            }
            KbCommand::Add {
                path_or_url,
                tags,
                recursive,
                ext,
            } => {
                return kb_add_http(path_or_url, tags, *recursive, ext).await;
            }
            _ => {}
        }
    }
    match cmd {
        KbCommand::Add {
            path_or_url,
            tags,
            recursive,
            ext,
        } => add(kb_root, path_or_url, tags, recursive, ext).await,
        KbCommand::Ls {
            tag,
            source_kind,
            limit,
        } => ls(kb_root, tag, source_kind, limit),
        KbCommand::Rm { doc_id, tag, yes } => rm(kb_root, doc_id, tag, yes),
        KbCommand::Search { query, k, json } => search(kb_root, query, k, json),
        KbCommand::Show { id } => show(kb_root, id),
        KbCommand::Visibility { doc_id, visibility } => set_visibility(kb_root, doc_id, visibility),
        KbCommand::Compact => compact(kb_root),
        KbCommand::Stats => stats(kb_root),
        KbCommand::Export { doc_id, to } => export(kb_root, doc_id, to),
        KbCommand::SyncAll {
            interval_min,
            max,
            dry_run,
        } => sync_all(kb_root, interval_min, max, dry_run).await,
        KbCommand::Eval { golden, k, verbose } => {
            eval_golden(kb_root, golden, k, verbose, gateway_up).await
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP routes — used when the gateway is running so the CLI sees the same
// live view as the agent runtime and writes land in the live store.
// ---------------------------------------------------------------------------

async fn kb_search_http(query: &str, k: usize, json_out: bool) -> Result<()> {
    let body = serde_json::json!({
        "query": query,
        "topK": k,
        "scoreThreshold": 0.0,
    });
    let resp: serde_json::Value =
        crate::cmd::gateway_http::post_json("/api/v1/knowledge/search", &body).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }
    let hits = resp.get("hits").and_then(|v| v.as_array());
    banner(&format!(
        "rsclaw kb search v{} (via http)",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));
    kv("query", &cyan(query));
    let count = hits.map(|a| a.len()).unwrap_or(0);
    kv("results", &bold(&count.to_string()));
    println!();
    if let Some(arr) = hits {
        for h in arr {
            let title = h
                .get("sourceTitle")
                .and_then(|v| v.as_str())
                .unwrap_or("(untitled)");
            let text = h.get("chunkText").and_then(|v| v.as_str()).unwrap_or("");
            let score = h.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let trimmed: String = text.chars().take(200).collect();
            println!(
                "  {} {} {}",
                dim(&format!("[{score:.3}]")),
                bold(title),
                trimmed
            );
        }
    }
    Ok(())
}

async fn kb_ls_http(
    tags: &[String],
    source_kind: Option<&str>,
    limit: usize,
) -> Result<()> {
    // Server takes a single optional `tag` query param; the CLI's
    // multi-tag UX is rare and not worth a multi-value endpoint.
    let mut path = format!("/api/v1/knowledge/docs?limit={limit}");
    if let Some(t) = tags.first().filter(|s| !s.is_empty()) {
        path.push_str(&format!(
            "&tag={}",
            urlencoding::encode(t),
        ));
    }
    if let Some(sk) = source_kind.filter(|s| !s.is_empty()) {
        path.push_str(&format!(
            "&source_kind={}",
            urlencoding::encode(sk),
        ));
    }
    let resp: serde_json::Value = crate::cmd::gateway_http::get_json(&path).await?;
    let docs = resp.get("docs").and_then(|v| v.as_array());
    if docs.is_none_or(|a| a.is_empty()) {
        println!("(no documents)");
        return Ok(());
    }
    println!(
        "{:<26} {:<8} v {:<5}  tags                            title",
        "doc_id", "kind", ""
    );
    for d in docs.unwrap() {
        let id = d.get("doc_id").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = d.get("source_kind").and_then(|v| v.as_str()).unwrap_or("?");
        let ver = d.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
        let tags: Vec<&str> = d
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        let title = d.get("title").and_then(|v| v.as_str()).unwrap_or("");
        println!(
            "{id:<26} {kind:<8} v {ver:<5}  {:<30}  {title}",
            tags.join(",")
        );
    }
    if let Some(next) = resp.get("nextCursor").and_then(|v| v.as_str()) {
        if !next.is_empty() {
            println!("\n(cursor for next page: {next})");
        }
    }
    Ok(())
}

async fn kb_rm_http(
    doc_id: Option<String>,
    tag: Option<String>,
    yes: bool,
) -> Result<()> {
    if !yes {
        eprintln!("Refusing to tombstone without --yes (this is a destructive operation).");
        return Ok(());
    }
    let (did, query) = match (doc_id, tag) {
        (Some(id), None) => (id, String::new()),
        // Bulk-by-tag: the `_bulk` segment is a placeholder for the route's
        // {doc_id} slot; the server prefers the `tag` query param when set.
        (None, Some(t)) => ("_bulk".to_owned(), format!("?tag={}", urlencoding::encode(&t))),
        (Some(_), Some(_)) => anyhow::bail!("pass either doc_id or --tag, not both"),
        (None, None) => anyhow::bail!("pass either a doc_id or --tag <name>"),
    };
    let path = format!("/api/v1/knowledge/docs/{did}{query}");
    let resp: serde_json::Value = crate::cmd::gateway_http::delete_json(&path).await?;
    if let Some(n) = resp.get("tombstoned").and_then(|v| v.as_u64()) {
        let tag = resp.get("tag").and_then(|v| v.as_str()).unwrap_or("?");
        println!("tombstoned {n} docs with tag={tag}");
    } else {
        println!("tombstoned {did}");
    }
    Ok(())
}

async fn kb_show_http(id: &str) -> Result<()> {
    // Try doc_id first; on 404 fall back to /search-style chunk fetch.
    let path = format!("/api/v1/knowledge/docs/{id}");
    match crate::cmd::gateway_http::get_json::<serde_json::Value>(&path).await {
        Ok(meta) => {
            println!("doc_id:    {}", meta.get("docId").and_then(|v| v.as_str()).unwrap_or("?"));
            println!("title:     {}", meta.get("title").and_then(|v| v.as_str()).unwrap_or(""));
            println!("kind:      {}", meta.get("sourceKind").and_then(|v| v.as_str()).unwrap_or("?"));
            println!("version:   {}", meta.get("version").and_then(|v| v.as_u64()).unwrap_or(0));
            if let Some(cid) = meta.get("collectionId").and_then(|v| v.as_str()) {
                println!("collection:{cid}");
            }
            if let Some(arr) = meta.get("tags").and_then(|v| v.as_array()) {
                let tags: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                if !tags.is_empty() {
                    println!("tags:      {}", tags.join(", "));
                }
            }
            let chunks_path = format!("/api/v1/knowledge/docs/{id}/chunks");
            let chunks: serde_json::Value =
                crate::cmd::gateway_http::get_json(&chunks_path).await?;
            let arr = chunks.get("chunks").and_then(|v| v.as_array());
            let n = arr.map(|a| a.len()).unwrap_or(0);
            println!("chunks:    {n}");
            println!("---");
            if let Some(arr) = arr {
                for c in arr {
                    let cid = c.get("chunkId").and_then(|v| v.as_str()).unwrap_or("?");
                    let head: Vec<&str> = c
                        .get("headingPath")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                        .unwrap_or_default();
                    let head_s = if head.is_empty() {
                        "(root)".to_owned()
                    } else {
                        head.join(" > ")
                    };
                    let text = c.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let snippet: String = text.chars().take(180).collect();
                    println!("[{cid}]  §{head_s}");
                    println!("    {snippet}");
                }
            }
            Ok(())
        }
        Err(e) => {
            // Likely doc_not_found — try treating id as chunk_id by hitting search.
            // The chunk-lookup HTTP surface is currently bound to the search
            // endpoint; emit the original error if id doesn't look like a chunk.
            if id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                eprintln!("doc-id lookup failed: {e}; chunk_id lookup over HTTP not yet exposed.");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

async fn kb_visibility_http(doc_id: &str, visibility: &str) -> Result<()> {
    let path = format!("/api/v1/knowledge/docs/{doc_id}/visibility");
    let body = serde_json::json!({ "visibility": visibility });
    let _: serde_json::Value =
        crate::cmd::gateway_http::patch_json(&path, &body).await?;
    println!("updated {doc_id} visibility → {visibility}");
    Ok(())
}

async fn kb_export_http(doc_id: &str, to: &std::path::Path) -> Result<()> {
    let path = format!("/api/v1/knowledge/docs/{doc_id}/content");
    let bytes = crate::cmd::gateway_http::get_bytes(&path).await?;
    std::fs::write(to, &bytes).with_context(|| format!("write {}", to.display()))?;
    println!("exported {doc_id} → {} ({} bytes)", to.display(), bytes.len());
    Ok(())
}

async fn kb_sync_all_http(interval_min: u64, max: usize, dry_run: bool) -> Result<()> {
    let body = serde_json::json!({
        "interval_min": interval_min,
        "max": max,
        "dry_run": dry_run,
    });
    let resp: serde_json::Value =
        crate::cmd::gateway_http::post_json("/api/v1/knowledge/sync-all", &body).await?;
    println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_default());
    Ok(())
}

async fn kb_compact_http() -> Result<()> {
    let empty = serde_json::json!({});
    let resp: serde_json::Value =
        crate::cmd::gateway_http::post_json("/api/v1/knowledge/compact", &empty).await?;
    banner(&format!(
        "rsclaw kb compact v{} (via http)",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));
    if let Some(obj) = resp.as_object() {
        for (k, v) in obj {
            kv(k, &bold(&v.to_string()));
        }
    }
    Ok(())
}

async fn kb_stats_http() -> Result<()> {
    let resp: serde_json::Value =
        crate::cmd::gateway_http::get_json("/api/v1/knowledge/stats").await?;
    banner(&format!(
        "rsclaw kb stats v{} (via http)",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));
    if let Some(obj) = resp.as_object() {
        for (k, v) in obj {
            kv(k, &bold(&v.to_string()));
        }
    }
    Ok(())
}

async fn kb_add_http(
    path_or_url: &str,
    tags: &[String],
    recursive: bool,
    _ext: &str,
) -> Result<()> {
    // The HTTP API ingest endpoints sit under a collection. For the
    // ergonomic `rsclaw kb add <thing>` form we resolve a default
    // collection: prefer one named "default" if it exists, otherwise
    // pick the first collection, otherwise create one called "default".
    let collections: serde_json::Value =
        crate::cmd::gateway_http::get_json("/api/v1/knowledge/collections").await?;
    let coll_id = pick_default_collection(&collections).await?;

    let is_url = path_or_url.starts_with("http://") || path_or_url.starts_with("https://");
    let (endpoint, body) = if is_url {
        (
            format!("/api/v1/knowledge/collections/{coll_id}/docs/from-url"),
            serde_json::json!({
                "url": path_or_url,
                "tags": tags,
            }),
        )
    } else {
        let abs = std::fs::canonicalize(path_or_url)
            .with_context(|| format!("canonicalize path {path_or_url}"))?;
        let endpoint = if abs.is_dir() {
            format!("/api/v1/knowledge/collections/{coll_id}/docs/from-dir")
        } else {
            format!("/api/v1/knowledge/collections/{coll_id}/docs/from-path")
        };
        (
            endpoint,
            serde_json::json!({
                "path": abs.to_string_lossy(),
                "tags": tags,
                "recursive": recursive,
            }),
        )
    };
    let resp: serde_json::Value = crate::cmd::gateway_http::post_json(&endpoint, &body).await?;
    ok(&format!(
        "queued ingest into {} → {}",
        bold(&coll_id),
        resp
    ));
    Ok(())
}

async fn pick_default_collection(list: &serde_json::Value) -> Result<String> {
    let arr = list
        .as_array()
        .or_else(|| list.get("collections").and_then(|v| v.as_array()))
        .ok_or_else(|| anyhow::anyhow!("unexpected /knowledge/collections response shape"))?;
    if let Some(c) = arr.iter().find(|c| {
        c.get("name").and_then(|v| v.as_str()) == Some("default")
            || c.get("id").and_then(|v| v.as_str()) == Some("default")
    }) {
        if let Some(id) = c.get("id").and_then(|v| v.as_str()) {
            return Ok(id.to_owned());
        }
    }
    if let Some(first) = arr.first()
        && let Some(id) = first.get("id").and_then(|v| v.as_str())
    {
        return Ok(id.to_owned());
    }
    // No collections exist yet — create one named "default".
    let resp: serde_json::Value = crate::cmd::gateway_http::post_json(
        "/api/v1/knowledge/collections",
        &serde_json::json!({ "name": "default" }),
    )
    .await?;
    resp.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| anyhow::anyhow!("created default collection but response had no id"))
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

    // Pick the embedder. Default: local BGE (bge-small-zh) if a model
    // dir exists under `<base_dir>/models/`. base_dir is kb_root's
    // parent (kb_root = <base_dir>/kb). Falls back to StubEmbedder
    // when no model is present, so the CLI works out-of-the-box on a
    // fresh install before `rsclaw models download`.
    let embedder = rsclaw_kb::embedder::resolve_embedder(kb_root);
    let dim = embedder.dimension();
    let index =
        Arc::new(KbIndex::open_and_rebuild_with_dim(&paths, &store, dim).context("open index")?);
    Ok(Handles {
        store,
        paths,
        index,
        embedder,
    })
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

    let is_url = path_or_url.starts_with("http://") || path_or_url.starts_with("https://");
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

fn ls(kb_root: PathBuf, tag: Vec<String>, source_kind: Option<String>, limit: usize) -> Result<()> {
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

fn rm(kb_root: PathBuf, doc_id: Option<String>, tag: Option<String>, yes: bool) -> Result<()> {
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
    let mut d =
        docs::get(&rtx, &doc_id)?.ok_or_else(|| anyhow::anyhow!("doc not found: {doc_id}"))?;
    drop(rtx);
    d.status = KbStatus::Tombstoned;
    let wtx = h.store.begin_write()?;
    docs::put(&wtx, &d)?;
    wtx.commit()?;
    println!("tombstoned {doc_id}");
    Ok(())
}

fn rm_by_tag(h: &Handles, tag: &str) -> Result<()> {
    use redb::ReadableTable;

    use rsclaw_kb::store::codec::decode;
    let rtx = h.store.begin_read()?;
    let mut to_tombstone: Vec<rsclaw_kb::model::KbDoc> = Vec::new();
    {
        let tbl = rtx.open_table(rsclaw_kb::store::schema::KB_DOCS)?;
        for entry in tbl.iter()? {
            let (_, v) = entry?;
            let d: rsclaw_kb::model::KbDoc = decode(v.value())?;
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

fn search(kb_root: PathBuf, query: String, k: usize, json: bool) -> Result<()> {
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
            query_instruction: None,
        },
        &CallerScope::default(),
    )?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&out).unwrap_or_else(|_| format!("{out:?}"))
        );
        return Ok(());
    }
    if out.results.is_empty() {
        println!("(no hits)");
        if !out.warnings.is_empty() {
            for w in &out.warnings {
                eprintln!("warning: {w}");
            }
        }
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
    for w in &out.warnings {
        eprintln!("warning: {w}");
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
    let doc = match rsclaw_kb::store::docs::get(&rtx, &id)? {
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
    let chunks_list = rsclaw_kb::store::chunks::chunks_for_logical(&rtx, &doc.logical_source_id)?;
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
    let mut d =
        docs::get(&rtx, &doc_id)?.ok_or_else(|| anyhow::anyhow!("doc not found: {doc_id}"))?;
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
                Some(KbVisibility::Agent {
                    agent_id: id.to_string(),
                })
            } else if let Some(id) = s.strip_prefix("channel:") {
                Some(KbVisibility::Channel {
                    channel_id: id.to_string(),
                })
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
    use redb::ReadableTable;

    use rsclaw_kb::store::codec::decode;
    let h = open_kb(&kb_root)?;
    let rtx = h.store.begin_read()?;
    let mut counts = serde_json::Map::new();
    // Per-status doc breakdown.
    let docs_tbl = rtx.open_table(rsclaw_kb::store::schema::KB_DOCS)?;
    let mut active = 0u64;
    let mut tombstoned = 0u64;
    for entry in docs_tbl.iter()? {
        let (_, v) = entry?;
        let d: rsclaw_kb::model::KbDoc = decode(v.value())?;
        match d.status {
            rsclaw_kb::model::KbStatus::Active => active += 1,
            rsclaw_kb::model::KbStatus::Tombstoned => tombstoned += 1,
            _ => {}
        }
    }
    counts.insert(
        "docs_active".into(),
        serde_json::Value::Number(active.into()),
    );
    counts.insert(
        "docs_tombstoned".into(),
        serde_json::Value::Number(tombstoned.into()),
    );
    // Other table sizes.
    for (name, td) in [
        ("kb_chunks", rsclaw_kb::store::schema::KB_CHUNKS),
        ("kb_ledger", rsclaw_kb::store::schema::KB_LEDGER),
        ("kb_jobs_by_id", rsclaw_kb::store::schema::KB_JOBS_BY_ID),
        ("kb_seen_items", rsclaw_kb::store::schema::KB_SEEN_ITEMS),
        ("kb_entities", rsclaw_kb::store::schema::KB_ENTITIES),
        ("kb_entity_index", rsclaw_kb::store::schema::KB_ENTITY_INDEX),
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

/// Re-sync every Active URL-sourced doc whose SyncState is older
/// than `interval_min` minutes. Cap concurrency at `max`. Spec §S's
/// long-term plan is a gateway-resident background task; until that
/// lands users (or their cron job) can call this CLI directly.
async fn sync_all(kb_root: PathBuf, interval_min: u64, max: usize, dry_run: bool) -> Result<()> {
    use redb::ReadableTable;

    use rsclaw_kb::{
        canonicalize::canonicalize_url,
        store::{codec::decode, schema::KB_DOCS, seen::get_sync_state},
        sync::{KbSourceSyncer, SyncContext, SyncReason, UrlSyncer},
    };

    let h = open_kb(&kb_root)?;
    let cutoff_ms = chrono::Utc::now().timestamp_millis() - (interval_min as i64) * 60_000;

    // 1. Collect candidate URLs from Active URL docs.
    let mut candidates: Vec<(String, Vec<String>)> = Vec::new(); // (url, tags)
    {
        let rtx = h.store.begin_read()?;
        let tbl = rtx.open_table(KB_DOCS)?;
        for entry in tbl.iter()? {
            let (_, v) = entry?;
            let d: rsclaw_kb::model::KbDoc = decode(v.value())?;
            if d.status != rsclaw_kb::model::KbStatus::Active {
                continue;
            }
            let url = match &d.source {
                rsclaw_kb::model::KbSource::Url { url, .. } => url.clone(),
                _ => continue,
            };
            let canonical = canonicalize_url(&url).unwrap_or(url);
            let last = get_sync_state(&rtx, &canonical)
                .ok()
                .flatten()
                .map(|s| s.last_sync_at)
                .unwrap_or(0);
            if last < cutoff_ms {
                candidates.push((canonical, d.tags.clone()));
            }
        }
    }
    // De-dup canonical URLs (the same URL can back multiple versions).
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.dedup_by(|a, b| a.0 == b.0);
    let total_candidates = candidates.len();
    let to_run: Vec<_> = candidates.into_iter().take(max).collect();

    if dry_run {
        println!(
            "{}",
            serde_json::json!({
                "dry_run": true,
                "candidates": total_candidates,
                "would_run": to_run.iter().map(|(u, _)| u).collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }

    let ctx = SyncContext {
        store: h.store.clone(),
        paths: h.paths.clone(),
        index: h.index.clone(),
        embedder: h.embedder.clone(),
    };
    let mut added = 0u64;
    let mut skipped = 0u64;
    let mut errored: Vec<String> = Vec::new();
    for (url, tags) in &to_run {
        let syncer = UrlSyncer {
            url: url.clone(),
            tags: tags.clone(),
        };
        match syncer.sync(&ctx, SyncReason::Periodic).await {
            Ok(o) => {
                added += o.docs_added as u64;
                skipped += o.docs_skipped as u64;
            }
            Err(e) => errored.push(format!("{url}: {e}")),
        }
    }
    drain_worker(&h)?;
    println!(
        "{}",
        serde_json::json!({
            "candidates": total_candidates,
            "ran": to_run.len(),
            "added": added,
            "skipped": skipped,
            "errors": errored,
        })
    );
    Ok(())
}

fn export(kb_root: PathBuf, doc_id: String, to: PathBuf) -> Result<()> {
    let h = open_kb(&kb_root)?;
    let rtx = h.store.begin_read()?;
    let doc =
        docs::get(&rtx, &doc_id)?.ok_or_else(|| anyhow::anyhow!("doc not found: {doc_id}"))?;
    if !doc.visible_to(&CallerScope::default()) {
        anyhow::bail!("doc not visible to current scope");
    }
    let abs = h.paths.root.join(&doc.markdown_path);
    let body = rsclaw_kb::content_store::read::read_doc_body(&abs)
        .with_context(|| format!("read {}", abs.display()))?;
    if let Some(parent) = to.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
    }
    std::fs::write(&to, &body).with_context(|| format!("write {}", to.display()))?;
    println!(
        "wrote {} ({} bytes) → {}",
        doc.title,
        body.len(),
        to.display()
    );
    Ok(())
}

fn search_ctx(h: &Handles) -> SearchCtx {
    SearchCtx {
        store: h.store.clone(),
        index: h.index.clone(),
        paths: h.paths.clone(),
        embedder: h.embedder.clone(),
        reranker: rsclaw_kb::rerank::KbReranker::from_config(),
    }
}

// ---------------------------------------------------------------------------
// Eval — retrieval golden-set harness
// ---------------------------------------------------------------------------

/// One golden retrieval case. A hit "satisfies" the case when it matches
/// ALL expectations present (they constrain the same hit).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvalCase {
    query: String,
    #[serde(default)]
    expect_doc_id: Option<String>,
    #[serde(default)]
    expect_title_contains: Option<String>,
    #[serde(default)]
    expect_text_contains: Option<String>,
}

/// Transport-normalized search hit for eval scoring.
struct EvalHit {
    doc_id: String,
    title: String,
    text: String,
}

/// 1-based rank of the first hit satisfying the case, if any.
fn eval_case_rank(case: &EvalCase, hits: &[EvalHit]) -> Option<usize> {
    hits.iter()
        .position(|h| {
            case.expect_doc_id.as_ref().is_none_or(|d| &h.doc_id == d)
                && case
                    .expect_title_contains
                    .as_ref()
                    .is_none_or(|t| h.title.to_lowercase().contains(&t.to_lowercase()))
                && case
                    .expect_text_contains
                    .as_ref()
                    .is_none_or(|t| h.text.to_lowercase().contains(&t.to_lowercase()))
        })
        .map(|i| i + 1)
}

/// `rsclaw kb eval <golden.json>` — measure retrieval quality before/after
/// any pipeline change (embedder, chunking, reranker). Uses the production
/// search path: HTTP when the gateway is up (live view, no redb lock fight),
/// direct store with the production query-instruction resolution otherwise.
async fn eval_golden(
    kb_root: PathBuf,
    golden: PathBuf,
    k: usize,
    verbose: bool,
    via_http: bool,
) -> Result<()> {
    let raw = std::fs::read_to_string(&golden)
        .with_context(|| format!("read golden file {}", golden.display()))?;
    let cases: Vec<EvalCase> = serde_json::from_str(&raw)
        .context("golden file must be a JSON array of {query, expect*} cases")?;
    if cases.is_empty() {
        anyhow::bail!("golden file has no cases");
    }
    for (i, c) in cases.iter().enumerate() {
        if c.expect_doc_id.is_none()
            && c.expect_title_contains.is_none()
            && c.expect_text_contains.is_none()
        {
            anyhow::bail!(
                "case {i} ({:?}) has no expectation — add expectDocId / \
                 expectTitleContains / expectTextContains",
                c.query
            );
        }
    }

    let direct = if via_http { None } else { Some(open_kb(&kb_root)?) };
    // Mirror the production query path: the gateway applies the configured
    // (or Qwen3-defaulted) query instruction; the direct path must too, or
    // the eval measures a pipeline nobody actually serves.
    let query_instruction = rsclaw_kb::embedder::effective_embed_config().and_then(|m| {
        rsclaw_embed::resolve_query_instruction(m.query_instruction, m.model.as_deref())
    });

    banner(&format!(
        "rsclaw kb eval v{} ({})",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"),
        if via_http { "via http" } else { "direct store" }
    ));
    kv("golden", &cyan(&golden.display().to_string()));
    kv("cases", &bold(&cases.len().to_string()));
    kv("k", &bold(&k.to_string()));
    println!();

    let mut hit1 = 0usize;
    let mut hitk = 0usize;
    let mut mrr_sum = 0f64;
    for (i, case) in cases.iter().enumerate() {
        let hits: Vec<EvalHit> = if via_http {
            let body = serde_json::json!({
                "query": case.query,
                "topK": k,
                "scoreThreshold": 0.0,
            });
            let resp: serde_json::Value =
                crate::cmd::gateway_http::post_json("/api/v1/knowledge/search", &body).await?;
            resp.get("hits")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|h| EvalHit {
                            doc_id: h
                                .get("docId")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_owned(),
                            title: h
                                .get("sourceTitle")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_owned(),
                            text: h
                                .get("chunkText")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_owned(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            let h = direct.as_ref().expect("direct handles opened above");
            let ctx = search_ctx(h);
            let out = kb_search::run(
                &ctx,
                kb_search::KbSearchInput {
                    query: case.query.clone(),
                    k,
                    filter: Default::default(),
                    mode: "hybrid".into(),
                    diversity: "mmr".into(),
                    mmr_lambda: 0.5,
                    boost_entities: vec![],
                    query_instruction: query_instruction.clone(),
                },
                &CallerScope::default(),
            )?;
            out.results
                .into_iter()
                .map(|r| EvalHit {
                    doc_id: r.doc_id,
                    title: r.doc_title,
                    text: r.text,
                })
                .collect()
        };

        match eval_case_rank(case, &hits) {
            Some(r) => {
                if r == 1 {
                    hit1 += 1;
                }
                hitk += 1;
                mrr_sum += 1.0 / r as f64;
                if verbose {
                    println!("  hit  [{}] rank {} — {}", i + 1, r, case.query);
                }
            }
            None => {
                println!("  {} [{}] {}", bold("MISS"), i + 1, cyan(&case.query));
                for (j, h) in hits.iter().take(3).enumerate() {
                    let snippet: String = h.text.chars().take(80).collect();
                    println!("        top{}: {} — {snippet}", j + 1, h.title);
                }
                if hits.is_empty() {
                    println!("        (no hits at all)");
                }
            }
        }
    }

    println!();
    let n = cases.len() as f64;
    kv(
        "hit@1",
        &bold(&format!(
            "{:.1}%  ({hit1}/{})",
            hit1 as f64 / n * 100.0,
            cases.len()
        )),
    );
    kv(
        &format!("hit@{k}"),
        &bold(&format!(
            "{:.1}%  ({hitk}/{})",
            hitk as f64 / n * 100.0,
            cases.len()
        )),
    );
    kv("MRR", &bold(&format!("{:.3}", mrr_sum / n)));
    Ok(())
}

#[cfg(test)]
mod eval_tests {
    use super::*;

    fn hit(doc_id: &str, title: &str, text: &str) -> EvalHit {
        EvalHit {
            doc_id: doc_id.into(),
            title: title.into(),
            text: text.into(),
        }
    }

    #[test]
    fn rank_matches_all_expectations_on_one_hit() {
        let case: EvalCase = serde_json::from_str(
            r#"{"query":"q","expectTitleContains":"蒙牛","expectTextContains":"12%"}"#,
        )
        .unwrap();
        let hits = vec![
            // Title matches but text doesn't — expectations constrain the
            // SAME hit, so this must not count.
            hit("d1", "蒙牛年报", "其他内容"),
            hit("d2", "蒙牛2025年报", "营收增长12%"),
        ];
        assert_eq!(eval_case_rank(&case, &hits), Some(2));
    }

    #[test]
    fn rank_is_case_insensitive_and_none_on_miss() {
        let case: EvalCase =
            serde_json::from_str(r#"{"query":"q","expectTitleContains":"mengniu"}"#).unwrap();
        assert_eq!(
            eval_case_rank(&case, &[hit("d", "Mengniu Report", "x")]),
            Some(1)
        );
        assert_eq!(eval_case_rank(&case, &[hit("d", "Other", "x")]), None);
    }

    #[test]
    fn rank_by_doc_id_exact() {
        let case: EvalCase =
            serde_json::from_str(r#"{"query":"q","expectDocId":"abc"}"#).unwrap();
        let hits = vec![hit("xyz", "t", "x"), hit("abc", "t", "x")];
        assert_eq!(eval_case_rank(&case, &hits), Some(2));
    }
}
