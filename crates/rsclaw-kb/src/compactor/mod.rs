//! Compactor: orphan file cleanup + ledger state advancement + physical
//! purge of tombstoned docs (chunks, doc row, markdown/raw files, dangling
//! entity edges). Designed to be safe-to-run-anytime; never deletes data
//! still referenced by a live doc. Each phase wraps state changes in single
//! write transactions. Index layers are cleaned by the caller from
//! `CompactStats::purged_chunk_ids` (see `KbIndex::apply_purge`).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::Result;

use crate::{
    ledger::LedgerStatus,
    model::{KbChunk, KbDoc, KbStatus, VersionPointer},
    paths::KbPaths,
    store::{
        KbStore,
        codec::decode,
        ledger,
        schema::{
            KB_CHUNK_BY_LOGICAL, KB_CHUNKS, KB_DOC_LATEST_VERSION, KB_DOCS, KB_ENTITY_INDEX,
        },
    },
};

pub const DEFAULT_GRACE_SECS: i64 = 3600; // 1h
pub const DEFAULT_RETENTION_SECS: i64 = 30 * 86400; // 30 days

#[derive(Debug, Clone, Default)]
pub struct CompactStats {
    pub orphans_deleted: usize,
    pub ledger_advanced_to_cleanup: usize,
    pub ledger_advanced_to_done: usize,
    /// Tombstoned docs physically removed this tick.
    pub docs_purged: usize,
    /// Chunk rows removed with those docs. The caller must drop these ids
    /// from the tantivy / HNSW caches (`KbIndex::apply_purge`).
    pub purged_chunk_ids: Vec<String>,
    /// Entity edges whose chunk no longer exists.
    pub entity_edges_removed: usize,
}

pub fn run_compactor_tick(store: &KbStore, paths: &KbPaths, now_ms: i64) -> Result<CompactStats> {
    let mut stats = CompactStats::default();
    let referenced = referenced_paths(store)?;
    let cutoff_secs = (now_ms / 1000) - DEFAULT_GRACE_SECS;
    let cutoff = if cutoff_secs > 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(cutoff_secs as u64)
    } else {
        SystemTime::UNIX_EPOCH
    };
    for dir in ["md", "raw"] {
        let abs_dir = paths.root.join(dir);
        if !abs_dir.exists() {
            continue;
        }
        stats.orphans_deleted += scan_and_delete_orphans(&abs_dir, dir, &referenced, cutoff)?;
    }

    {
        let rtx = store.begin_read()?;
        let candidates = ledger::list_by_status(&rtx, LedgerStatus::IndexingComplete)?;
        drop(rtx);
        for entry in candidates {
            for rel in &entry.old_paths {
                // Content-addressed markdown paths repeat when a source
                // reverts to earlier content: the superseded version's
                // "old" path can be the current doc's path again.
                if referenced.contains(rel) {
                    continue;
                }
                let abs = paths.root.join(rel);
                if abs.exists() {
                    if let Err(e) = std::fs::remove_file(&abs) {
                        tracing::warn!(
                            path = %abs.display(),
                            "kb compactor: failed to remove superseded file: {e}"
                        );
                    }
                }
            }
            let wtx = store.begin_write()?;
            ledger::update_status(&wtx, &entry.id, LedgerStatus::CleanupPending, now_ms)?;
            wtx.commit()?;
            stats.ledger_advanced_to_cleanup += 1;
        }
    }

    {
        let rtx = store.begin_read()?;
        let candidates = ledger::list_by_status(&rtx, LedgerStatus::CleanupPending)?;
        drop(rtx);
        let retention_ms = DEFAULT_RETENTION_SECS * 1000;
        for entry in candidates {
            if now_ms - entry.updated_at > retention_ms {
                let wtx = store.begin_write()?;
                ledger::update_status(&wtx, &entry.id, LedgerStatus::Done, now_ms)?;
                wtx.commit()?;
                stats.ledger_advanced_to_done += 1;
            }
        }
    }

    purge_tombstoned(store, paths, now_ms, &mut stats)?;
    stats.entity_edges_removed = remove_dangling_entity_edges(store)?;

    tracing::info!(
        orphans = stats.orphans_deleted,
        cleanup = stats.ledger_advanced_to_cleanup,
        done = stats.ledger_advanced_to_done,
        purged_docs = stats.docs_purged,
        purged_chunks = stats.purged_chunk_ids.len(),
        entity_edges = stats.entity_edges_removed,
        "kb compactor: tick complete"
    );
    Ok(stats)
}

/// Physically remove docs tombstoned longer than `DEFAULT_GRACE_SECS`: their
/// chunk rows (+ by-logical index), the doc row, a latest-version pointer
/// still aiming at them, and their markdown/raw files when no live doc
/// shares the (content-addressed) path. Deleted content must not linger on
/// disk or in search caches indefinitely.
fn purge_tombstoned(
    store: &KbStore,
    paths: &KbPaths,
    now_ms: i64,
    stats: &mut CompactStats,
) -> Result<()> {
    use redb::ReadableTable;
    let grace_ms = DEFAULT_GRACE_SECS * 1000;
    let victims: Vec<String> = {
        let rtx = store.begin_read()?;
        let tbl = rtx.open_table(KB_DOCS)?;
        let mut out = Vec::new();
        for entry in tbl.iter()? {
            let (k, v) = entry?;
            match decode::<KbDoc>(v.value()) {
                Ok(d) if d.status == KbStatus::Tombstoned && now_ms - d.updated_at > grace_ms => {
                    out.push(d.id)
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(doc = %k.value(), "kb compactor: skip undecodable doc: {e:#}")
                }
            }
        }
        out
    };
    if victims.is_empty() {
        return Ok(());
    }

    let mut victim_files: Vec<String> = Vec::new();
    let wtx = store.begin_write()?;
    for doc_id in &victims {
        // Re-check inside the write tx: the doc may have been resurrected
        // by a re-ingest since the read snapshot.
        let doc: KbDoc = {
            let tbl = wtx.open_table(KB_DOCS)?;
            let Some(v) = tbl.get(doc_id.as_str())? else {
                continue;
            };
            match decode(v.value()) {
                Ok(d) => d,
                Err(_) => continue,
            }
        };
        if doc.status != KbStatus::Tombstoned {
            continue;
        }
        let prefix = format!("{}\0", doc.logical_source_id);
        let end = format!("{}\u{1}", doc.logical_source_id);
        let chunk_ids: Vec<String> = {
            let idx = wtx.open_table(KB_CHUNK_BY_LOGICAL)?;
            let mut ids = Vec::new();
            for entry in idx.range(prefix.as_str()..end.as_str())? {
                let (k, _) = entry?;
                if let Some(id) = k.value().strip_prefix(prefix.as_str()) {
                    ids.push(id.to_owned());
                }
            }
            ids
        };
        let mut owned: Vec<String> = Vec::new();
        {
            let tbl = wtx.open_table(KB_CHUNKS)?;
            for id in &chunk_ids {
                if let Some(v) = tbl.get(id.as_str())? {
                    // Chunk ids are shared across versions of one source;
                    // only drop rows still owned by the purged doc.
                    match decode::<KbChunk>(v.value()) {
                        Ok(c) if c.doc_id == doc.id => owned.push(id.clone()),
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(chunk = %id, "kb compactor: skip undecodable chunk: {e:#}")
                        }
                    }
                }
            }
        }
        {
            let mut tbl = wtx.open_table(KB_CHUNKS)?;
            for id in &owned {
                tbl.remove(id.as_str())?;
            }
        }
        {
            let mut idx = wtx.open_table(KB_CHUNK_BY_LOGICAL)?;
            for id in &owned {
                idx.remove(format!("{prefix}{id}").as_str())?;
            }
        }
        {
            let mut latest = wtx.open_table(KB_DOC_LATEST_VERSION)?;
            let points_here = match latest.get(doc.logical_source_id.as_str())? {
                Some(v) => decode::<VersionPointer>(v.value())
                    .map(|p| p.doc_id == doc.id)
                    .unwrap_or(false),
                None => false,
            };
            if points_here {
                latest.remove(doc.logical_source_id.as_str())?;
            }
        }
        {
            let mut tbl = wtx.open_table(KB_DOCS)?;
            tbl.remove(doc.id.as_str())?;
        }
        victim_files.push(doc.markdown_path.clone());
        if let Some(r) = doc.raw_path.clone() {
            victim_files.push(r);
        }
        stats.docs_purged += 1;
        stats.purged_chunk_ids.extend(owned);
    }
    wtx.commit()?;

    // Files go only after the rows are gone, and only when no remaining
    // doc (or in-flight ledger entry) still points at the same path.
    let still_referenced = referenced_paths(store)?;
    for rel in victim_files {
        if still_referenced.contains(&rel) {
            continue;
        }
        let abs = paths.root.join(&rel);
        if abs.exists()
            && let Err(e) = std::fs::remove_file(&abs)
        {
            tracing::warn!(path = %abs.display(), "kb compactor: failed to remove purged file: {e}");
        }
    }
    Ok(())
}

/// Remove entity↔chunk edges whose chunk row no longer exists (purged docs,
/// superseded versions). Returns the number of edges removed.
fn remove_dangling_entity_edges(store: &KbStore) -> Result<usize> {
    use redb::ReadableTable;
    let dangling: Vec<String> = {
        let rtx = store.begin_read()?;
        let edges = rtx.open_table(KB_ENTITY_INDEX)?;
        let chunks = rtx.open_table(KB_CHUNKS)?;
        let mut out = Vec::new();
        for entry in edges.iter()? {
            let (k, _) = entry?;
            let key = k.value();
            // key = "{entity_id}\0{chunk_id}"
            let Some((_, chunk_id)) = key.split_once('\0') else {
                continue;
            };
            if chunks.get(chunk_id)?.is_none() {
                out.push(key.to_owned());
            }
        }
        out
    };
    if dangling.is_empty() {
        return Ok(0);
    }
    let wtx = store.begin_write()?;
    {
        let mut edges = wtx.open_table(KB_ENTITY_INDEX)?;
        for key in &dangling {
            edges.remove(key.as_str())?;
        }
    }
    wtx.commit()?;
    Ok(dangling.len())
}

fn referenced_paths(store: &KbStore) -> Result<HashSet<String>> {
    use redb::ReadableTable;
    let rtx = store.begin_read()?;
    let mut out = HashSet::new();
    for status in [LedgerStatus::Pending, LedgerStatus::IndexingComplete] {
        for e in ledger::list_by_status(&rtx, status)? {
            for p in e.new_paths {
                out.insert(p);
            }
        }
    }
    let tbl = rtx.open_table(KB_DOCS)?;
    for entry in tbl.iter()? {
        let (_, v) = entry?;
        let d: KbDoc = decode(v.value())?;
        out.insert(d.markdown_path);
        if let Some(r) = d.raw_path {
            out.insert(r);
        }
    }
    Ok(out)
}

fn scan_and_delete_orphans(
    abs_dir: &Path,
    rel_prefix: &str,
    referenced: &HashSet<String>,
    cutoff: SystemTime,
) -> Result<usize> {
    let mut deleted = 0;
    let mut stack: Vec<PathBuf> = vec![abs_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let rel = match path.strip_prefix(abs_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let rel_str = format!("{rel_prefix}/{}", rel.display());
            if referenced.contains(&rel_str) {
                continue;
            }
            if let Ok(meta) = path.metadata() {
                if let Ok(mtime) = meta.modified() {
                    if mtime >= cutoff {
                        continue;
                    }
                }
            }
            if std::fs::remove_file(&path).is_ok() {
                deleted += 1;
            }
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn empty_store_runs_clean() {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let paths = KbPaths::new(tmp.path().join("kb"));
        paths.ensure_layout().unwrap();
        let stats = run_compactor_tick(&store, &paths, 0).unwrap();
        assert_eq!(stats.orphans_deleted, 0);
    }

    #[test]
    fn orphan_outside_grace_is_deleted() {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let paths = KbPaths::new(tmp.path().join("kb"));
        paths.ensure_layout().unwrap();
        let orphan = paths.root.join("md/doc/orphan--ffffffff--ffffffff.md");
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, "stale").unwrap();
        // Far-future now_ms makes the on-disk mtime older than cutoff.
        let now = chrono::Utc::now().timestamp_millis() + 86_400_000;
        let stats = run_compactor_tick(&store, &paths, now).unwrap();
        assert_eq!(stats.orphans_deleted, 1);
        assert!(!orphan.exists());
    }

    #[test]
    fn referenced_file_preserved() {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let paths = KbPaths::new(tmp.path().join("kb"));
        paths.ensure_layout().unwrap();
        let rel = "md/doc/keep--aaaaaaaa--bbbbbbbb.md".to_string();
        let abs = paths.root.join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "important").unwrap();
        let doc = KbDoc {
            id: "d1".into(),
            logical_source_id: "lsid".into(),
            source: crate::model::KbSource::Doc { path: "/x".into() },
            source_kind: crate::model::KbSourceKind::Doc,
            title: "T".into(),
            mime: "text/markdown".into(),
            raw_sha256: "sha".into(),
            markdown_path: rel.clone(),
            markdown_sha256: "md".into(),
            raw_path: None,
            owner_user_id: None,
            created_at: 0,
            updated_at: 0,
            version: 1,
            status: crate::model::KbStatus::Active,
            visibility: crate::model::KbVisibility::Global,
            tags: vec![],
            meta: serde_json::Value::Null,
        };
        {
            let wtx = store.begin_write().unwrap();
            crate::store::docs::put(&wtx, &doc).unwrap();
            wtx.commit().unwrap();
        }
        let now = chrono::Utc::now().timestamp_millis() + 86_400_000;
        let stats = run_compactor_tick(&store, &paths, now).unwrap();
        assert_eq!(stats.orphans_deleted, 0);
        assert!(abs.exists());
    }

    #[test]
    fn tombstoned_doc_is_physically_purged() {
        use std::sync::Arc;

        use crate::{
            canonicalize::{CanonicalizeInput, canonicalize_by_mime},
            embedder::{KbEmbedder, StubEmbedder},
            pipeline::{IngestInput, ingest_canonicalized},
            worker::{DefaultDispatcher, WorkerConfig, WorkerPool, handlers::HandlerCtx},
        };

        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let bytes = b"# Secret\n\nprivate body text.";
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes,
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: None,
        })
        .unwrap()
        .unwrap();
        let out = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon,
                raw_bytes: bytes,
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
        let index = Arc::new(crate::index::KbIndex::open(&paths).unwrap());
        let ctx = HandlerCtx {
            store: store.clone(),
            paths: paths.clone(),
            embedder,
            index,
        };
        let cfg = WorkerConfig {
            worker_id: "w".into(),
            ..WorkerConfig::default()
        };
        WorkerPool::run_one_blocking(&ctx, &cfg, &DefaultDispatcher).unwrap();

        let md_abs = paths.root.join(&out.markdown_rel_path);
        assert!(md_abs.exists());
        {
            let rtx = store.begin_read().unwrap();
            let mut d = crate::store::docs::get(&rtx, &out.doc_id).unwrap().unwrap();
            drop(rtx);
            d.status = KbStatus::Tombstoned;
            d.updated_at = 0;
            let wtx = store.begin_write().unwrap();
            crate::store::docs::put(&wtx, &d).unwrap();
            wtx.commit().unwrap();
        }
        let now = chrono::Utc::now().timestamp_millis() + 86_400_000;
        let stats = run_compactor_tick(&store, &paths, now).unwrap();
        assert_eq!(stats.docs_purged, 1);
        assert!(!stats.purged_chunk_ids.is_empty());
        assert!(!md_abs.exists(), "markdown of purged doc must be deleted");
        let rtx = store.begin_read().unwrap();
        assert!(crate::store::docs::get(&rtx, &out.doc_id).unwrap().is_none());
        for id in &stats.purged_chunk_ids {
            assert!(crate::store::chunks::get(&rtx, id).unwrap().is_none());
        }
    }
}
