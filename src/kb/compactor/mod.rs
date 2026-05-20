//! Compactor: orphan file cleanup + ledger state advancement.
//! Designed to be safe-to-run-anytime; never deletes data still
//! referenced by the latest version. Each phase wraps state changes
//! in single write transactions.

use crate::kb::ledger::LedgerStatus;
use crate::kb::model::KbDoc;
use crate::kb::paths::KbPaths;
use crate::kb::store::codec::decode;
use crate::kb::store::schema::KB_DOCS;
use crate::kb::store::{ledger, KbStore};
use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const DEFAULT_GRACE_SECS: i64 = 3600; // 1h
pub const DEFAULT_RETENTION_SECS: i64 = 30 * 86400; // 30 days

#[derive(Debug, Clone, Default)]
pub struct CompactStats {
    pub orphans_deleted: usize,
    pub ledger_advanced_to_cleanup: usize,
    pub ledger_advanced_to_done: usize,
}

pub fn run_compactor_tick(
    store: &KbStore,
    paths: &KbPaths,
    now_ms: i64,
) -> Result<CompactStats> {
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

    tracing::info!(
        orphans = stats.orphans_deleted,
        cleanup = stats.ledger_advanced_to_cleanup,
        done = stats.ledger_advanced_to_done,
        "kb compactor: tick complete"
    );
    Ok(stats)
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
    use super::*;
    use tempfile::TempDir;

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
            source: crate::kb::model::KbSource::Doc { path: "/x".into() },
            source_kind: crate::kb::model::KbSourceKind::Doc,
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
            status: crate::kb::model::KbStatus::Active,
            visibility: crate::kb::model::KbVisibility::Global,
            tags: vec![],
            meta: serde_json::Value::Null,
        };
        {
            let wtx = store.begin_write().unwrap();
            crate::kb::store::docs::put(&wtx, &doc).unwrap();
            wtx.commit().unwrap();
        }
        let now = chrono::Utc::now().timestamp_millis() + 86_400_000;
        let stats = run_compactor_tick(&store, &paths, now).unwrap();
        assert_eq!(stats.orphans_deleted, 0);
        assert!(abs.exists());
    }
}
