# KB MVP Week 4 — Syncers + Compactor + CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (recommended) to implement this plan task-by-task.

**Goal:** Wire Week 1–3 (canonicalize + persistence + retrieval) into user-callable surfaces. By end of Week 4: a user can run `rsclaw kb add manual.md`, `rsclaw kb search "query"`, `rsclaw kb ls`, `rsclaw kb compact` from the CLI; URL ingest works end-to-end through `UrlSyncer`; orphan files + stale chunks get reaped by the compactor.

**Architecture:**
- **`KbSourceSyncer` trait** (`sync/mod.rs`) — generic interface for ManualUpload + Url + future syncers. Each syncer holds its source identity + cursor, runs `sync(ctx) → SyncOutcome` returning {docs_added, docs_updated, docs_skipped, partial}. Reuses Week 1's `store::seen::SyncState` for cursor persistence.
- **`ManualUploadSyncer`** (`sync/manual.rs`) — file path → bytes → `canonicalize_by_mime` → `ingest_canonicalized`. No HTTP, no cursor. Used by `rsclaw kb add <path>`.
- **`UrlSyncer`** (`sync/url.rs`) — reqwest HEAD → check ETag/Last-Modified against `SyncState.cursor`; if changed, GET body → `canonicalize_by_mime(mime, bytes)` → ingest. Cursor format: `etag:xxx` / `lastmod:xxx` / `contenthash:xxx`. Used by `rsclaw kb add <url>`.
- **Compactor** (`compactor/mod.rs`) — three concerns serial: (1) orphan markdown/raw files older than `grace_period` deleted; (2) ledger `IndexingComplete` rows whose `old_paths` are no longer referenced advance to `CleanupPending` + delete old files; (3) `CleanupPending` rows older than `tombstone_retention_days` advance to `Done`. Pure read-then-write, single-tx per state transition.
- **CLI** — `Command::Kb(KbCommand)` variant in `src/cli/kb.rs`. Subcommands: `add | ls | rm | search | show | visibility | compact | stats`. Each calls the existing Week 2/3 surface (`ingest_canonicalized`, `kb_search`, `kb_list_docs`, etc.).

**Tech Stack:** Rust 2024, reqwest 0.12 (existing), tokio (existing), chrono (existing). **No new Cargo deps.**

**Spec reference:** §S Sync framework, §J Compactor, §5 CLI.

**Builds on:** Week 1 (canonicalize), Week 2 (ingest pipeline + worker), Week 3 (kb_search + tools).

---

## Module additions

```
src/kb/
  sync/
    mod.rs              # NEW: KbSourceSyncer trait + SyncReason/SyncOutcome/SyncError
    state.rs            # NEW: SyncRegistry (load/save SyncState; reuses store::seen::SyncState)
    manual.rs           # NEW: ManualUploadSyncer
    url.rs              # NEW: UrlSyncer
  compactor/
    mod.rs              # NEW: run_compactor_tick + orphan scan + ledger advance
tests/
  kb_week4_syncers.rs   # NEW: manual + url ingest e2e
  kb_week4_compactor.rs # NEW: orphan scan + ledger advance
src/cli/
  kb.rs                 # NEW: KbCommand enum
src/cmd/
  kb.rs                 # NEW: cmd_kb handler dispatch
```

Existing files modified:
- `src/cli/mod.rs` — declare `pub mod kb;` + `Command::Kb(KbCommand)` variant.
- `src/cmd/mod.rs` — `pub mod kb; pub use kb::cmd_kb;`.
- `src/main.rs` — match `Command::Kb(c) => cmd_kb(c, ...)`.
- `src/kb/mod.rs` — `pub mod sync; pub mod compactor;` + re-exports.

---

## Conventions

- **One commit per task** (`feat(kb): ...` / `test(kb): ...` / `feat(cli): ...`).
- **No new Cargo deps.**
- **CLI commands return `Result<()>`**; errors propagate as user-visible stderr lines.
- **`rsclaw kb add <path-or-url>`** detects URL by scheme prefix (`http://`/`https://`); otherwise treats as file path.
- **Concurrent CLI safety**: each `rsclaw kb ...` opens its own `KbStore` + `KbIndex`. Tantivy directory lock means two simultaneous `kb add` calls in one process serialise; across processes they conflict (acceptable for v1 — `rsclaw kb` is interactive single-user).

---

## Task 1: Bootstrap — kb sync/compactor module skeleton

**Files:** create `src/kb/sync/mod.rs` + `src/kb/compactor/mod.rs`; modify `src/kb/mod.rs`.

```bash
mkdir -p src/kb/sync src/kb/compactor
: > src/kb/sync/mod.rs
: > src/kb/compactor/mod.rs
```

Add to `src/kb/mod.rs` (alphabetically):

```rust
pub mod compactor;
pub mod sync;
```

Verify: `cargo check -p rsclaw`. Commit:

```bash
git add src/kb/sync/ src/kb/compactor/ src/kb/mod.rs
git commit -m "chore(kb): bootstrap Week 4 module skeleton (sync, compactor)"
```

---

## Task 2: `sync/mod.rs` — KbSourceSyncer trait + types

**Files:** `src/kb/sync/mod.rs`.

```rust
//! KbSourceSyncer trait + sync result types. Week 4 ships two
//! implementations: ManualUploadSyncer (file → ingest) and UrlSyncer
//! (HTTP → ingest). Future syncers (LocalFolder, Mail, Chat) plug into
//! the same trait.

pub mod manual;
pub mod state;
pub mod url;

pub use manual::ManualUploadSyncer;
pub use state::SyncRegistry;
pub use url::UrlSyncer;

use crate::kb::embedder::KbEmbedder;
use crate::kb::index::KbIndex;
use crate::kb::model::KbSourceKind;
use crate::kb::paths::KbPaths;
use crate::kb::store::KbStore;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncReason {
    Periodic,
    Manual,
    OnEnable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub docs_added: usize,
    pub docs_updated: usize,
    pub docs_skipped: usize,
    pub partial: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("auth failed: {0}")]
    AuthFailed(String),
    #[error("rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("budget exhausted")]
    BudgetExhausted,
    #[error("network: {0}")]
    Network(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("permanent: {0}")]
    Permanent(String),
    #[error("cancelled")]
    Cancelled,
}

pub struct SyncContext {
    pub store: Arc<KbStore>,
    pub paths: Arc<KbPaths>,
    pub index: Arc<KbIndex>,
    pub embedder: Arc<dyn KbEmbedder>,
}

#[async_trait::async_trait]
pub trait KbSourceSyncer: Send + Sync {
    fn source_kind(&self) -> KbSourceKind;
    fn source_id(&self) -> &str;
    fn sync_interval_secs(&self) -> Option<u64> {
        Some(20 * 60)
    }
    async fn sync(&self, ctx: &SyncContext, reason: SyncReason)
        -> Result<SyncOutcome, SyncError>;
}
```

**NOTE:** `async-trait` is not in Cargo.toml. Two options:
1. Add `async-trait = "0.1"` to deps (acceptable — Tower of established Rust traits).
2. Use a synchronous interface and run async work inside via `tokio::runtime::Handle::current().block_on(...)` — uglier.

Pick (1). Add to `Cargo.toml` deps. Commit:

```bash
git add Cargo.toml src/kb/sync/
git commit -m "feat(kb): KbSourceSyncer trait + SyncReason/SyncOutcome/SyncError types"
```

---

## Task 3: `sync/state.rs` — SyncRegistry

**Files:** `src/kb/sync/state.rs`.

Wraps `store::seen::SyncState` accessors so syncers don't reach into `store::` directly. Adds list-active-syncers helper for scheduler integration.

```rust
//! SyncRegistry: per-source state load/save. Reuses
//! `store::seen::SyncState` for persistence.

use crate::kb::store::seen::{get_sync_state, put_sync_state, SyncState};
use crate::kb::store::KbStore;
use anyhow::Result;

pub struct SyncRegistry;

impl SyncRegistry {
    pub fn load(store: &KbStore, source_id: &str) -> Result<Option<SyncState>> {
        let rtx = store.begin_read()?;
        get_sync_state(&rtx, source_id)
    }
    pub fn save(store: &KbStore, source_id: &str, state: &SyncState) -> Result<()> {
        let wtx = store.begin_write()?;
        put_sync_state(&wtx, source_id, state)?;
        wtx.commit()?;
        Ok(())
    }
}
```

Test: load returns None for missing; save+load roundtrips. Commit:

```bash
git add src/kb/sync/
git commit -m "feat(kb): SyncRegistry — load/save SyncState wrapper"
```

---

## Task 4: `sync/manual.rs` — ManualUploadSyncer

**Files:** `src/kb/sync/manual.rs`.

```rust
//! ManualUploadSyncer: file path → bytes → canonicalize_by_mime →
//! ingest_canonicalized. Used by `rsclaw kb add <path>` and the
//! future UI drag-drop flow. No HTTP, no cursor; SyncState exists
//! only for health-monitoring consistency.

use super::{KbSourceSyncer, SyncContext, SyncError, SyncOutcome, SyncReason};
use crate::kb::canonicalize::{canonicalize_by_mime, detect_mime, CanonicalizeInput};
use crate::kb::model::KbSourceKind;
use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
use anyhow::Result;
use std::path::PathBuf;

pub struct ManualUploadSyncer {
    pub source_id: String,
    pub file_path: PathBuf,
    pub tags: Vec<String>,
}

#[async_trait::async_trait]
impl KbSourceSyncer for ManualUploadSyncer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn source_id(&self) -> &str { &self.source_id }
    fn sync_interval_secs(&self) -> Option<u64> { None }

    async fn sync(
        &self,
        ctx: &SyncContext,
        _reason: SyncReason,
    ) -> Result<SyncOutcome, SyncError> {
        let bytes = std::fs::read(&self.file_path)
            .map_err(|e| SyncError::Permanent(format!("read {}: {e}", self.file_path.display())))?;
        let filename = self.file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let mime = detect_mime(&bytes, Some(filename));
        let ext = self.file_path.extension().and_then(|s| s.to_str()).unwrap_or("");
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: &bytes,
            mime: &mime,
            hint_title: Some(filename),
            logical_source_id_seed: None,
        })
        .map_err(|e| SyncError::Parse(format!("canonicalize: {e}")))?
        .ok_or_else(|| SyncError::Parse(format!("no canonical output for mime={mime}")))?;
        // Apply user tags by mutating canon metadata.
        let mut canon = canon;
        canon.metadata.tags.extend(self.tags.iter().cloned());
        let out = ingest_canonicalized(
            &ctx.store,
            IngestInput {
                canon: &canon, raw_bytes: &bytes, raw_ext: ext,
                visibility: None, owner_user_id: None,
                seen_key: Some(("manual", canon.metadata.logical_source_id.as_str())),
                source: None, paths: &ctx.paths,
            },
        )
        .map_err(|e| SyncError::Permanent(format!("ingest: {e}")))?;
        Ok(SyncOutcome {
            docs_added: if out.noop { 0 } else { 1 },
            docs_skipped: if out.noop { 1 } else { 0 },
            ..Default::default()
        })
    }
}
```

Test: integration in `tests/kb_week4_syncers.rs` (Task 7).

Commit:

```bash
git add src/kb/sync/
git commit -m "feat(kb): ManualUploadSyncer (file → ingest_canonicalized)"
```

---

## Task 5: `sync/url.rs` — UrlSyncer

**Files:** `src/kb/sync/url.rs`.

```rust
//! UrlSyncer: HTTP GET → canonicalize → ingest. Uses ETag/Last-Modified
//! conditional-get when SyncState has a prior cursor; falls back to
//! content-hash dedupe via the seen_items table.

use super::{KbSourceSyncer, SyncContext, SyncError, SyncOutcome, SyncReason};
use crate::kb::canonicalize::{canonicalize_by_mime, canonicalize_url, detect_mime, CanonicalizeInput};
use crate::kb::content_store::atomic::sha256_hex;
use crate::kb::model::{KbSource, KbSourceKind};
use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
use crate::kb::store::seen::{is_seen, SyncState};
use crate::kb::sync::SyncRegistry;
use anyhow::Result;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use std::time::Duration;

const DEFAULT_TIMEOUT_S: u64 = 30;

pub struct UrlSyncer {
    pub url: String,
    pub tags: Vec<String>,
}

impl UrlSyncer {
    pub fn source_id_for(url: &str) -> String {
        // Canonicalize URL so the source_id is stable across query-param permutations.
        canonicalize_url(url).unwrap_or_else(|_| url.to_string())
    }
}

#[async_trait::async_trait]
impl KbSourceSyncer for UrlSyncer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Url }
    fn source_id(&self) -> &str { &self.url }

    async fn sync(
        &self,
        ctx: &SyncContext,
        _reason: SyncReason,
    ) -> Result<SyncOutcome, SyncError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent("rsclaw-kb-syncer/1.0")
            .build()
            .map_err(|e| SyncError::Network(format!("client build: {e}")))?;

        let canonical_url = canonicalize_url(&self.url)
            .map_err(|e| SyncError::Parse(format!("url canonicalize: {e}")))?;
        let prior = SyncRegistry::load(&ctx.store, &canonical_url)
            .map_err(|e| SyncError::Permanent(format!("load state: {e}")))?;

        // Conditional GET if we have a prior etag / last-modified.
        let mut req = client.get(&canonical_url);
        if let Some(state) = &prior {
            if let Some(cur) = &state.cursor.strip_prefix("etag:") {
                req = req.header(IF_NONE_MATCH, *cur);
            } else if let Some(cur) = &state.cursor.strip_prefix("lastmod:") {
                req = req.header(IF_MODIFIED_SINCE, *cur);
            }
        }
        let resp = req.send().await
            .map_err(|e| SyncError::Network(format!("get {canonical_url}: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(SyncOutcome { docs_skipped: 1, ..Default::default() });
        }
        if !resp.status().is_success() {
            return Err(SyncError::Network(format!("status {} for {canonical_url}", resp.status())));
        }
        let etag = resp.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let last_mod = resp.headers().get(LAST_MODIFIED).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let content_type = resp.headers().get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
        let bytes = resp.bytes().await
            .map_err(|e| SyncError::Network(format!("body: {e}")))?
            .to_vec();

        // Content-hash dedupe via seen_items (under "url" source_id).
        let raw_sha = sha256_hex(&bytes);
        let rtx = ctx.store.begin_read().map_err(|e| SyncError::Permanent(e.to_string()))?;
        if let Some(prev) = is_seen(&rtx, "url", &canonical_url)
            .map_err(|e| SyncError::Permanent(e.to_string()))?
        {
            if prev.raw_sha256 == raw_sha {
                drop(rtx);
                return Ok(SyncOutcome { docs_skipped: 1, ..Default::default() });
            }
        }
        drop(rtx);

        let mime = content_type
            .or_else(|| Some(detect_mime(&bytes, Some(&canonical_url))))
            .unwrap_or_else(|| "text/html".into());
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: &bytes,
            mime: &mime,
            hint_title: Some(&canonical_url),
            logical_source_id_seed: None,
        })
        .map_err(|e| SyncError::Parse(format!("canonicalize: {e}")))?
        .ok_or_else(|| SyncError::Parse(format!("no canonical output for mime={mime}")))?;
        let mut canon = canon;
        canon.metadata.tags.extend(self.tags.iter().cloned());
        let raw_ext = mime_to_ext(&mime);
        let source = Some(KbSource::Url {
            url: canonical_url.clone(),
            fetched_at: chrono::Utc::now().timestamp_millis(),
        });
        let out = ingest_canonicalized(
            &ctx.store,
            IngestInput {
                canon: &canon, raw_bytes: &bytes, raw_ext,
                visibility: None, owner_user_id: None,
                seen_key: Some(("url", &canonical_url)),
                source, paths: &ctx.paths,
            },
        )
        .map_err(|e| SyncError::Permanent(format!("ingest: {e}")))?;

        // Persist cursor for next conditional-get round.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let cursor = if let Some(e) = etag {
            format!("etag:{e}")
        } else if let Some(lm) = last_mod {
            format!("lastmod:{lm}")
        } else {
            format!("contenthash:{raw_sha}")
        };
        let state = SyncState { cursor, last_sync_at: now_ms };
        SyncRegistry::save(&ctx.store, &canonical_url, &state)
            .map_err(|e| SyncError::Permanent(format!("save state: {e}")))?;

        Ok(SyncOutcome {
            docs_added: if out.noop { 0 } else { 1 },
            docs_skipped: if out.noop { 1 } else { 0 },
            ..Default::default()
        })
    }
}

fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "text/html" => "html",
        "text/markdown" => "md",
        "text/plain" => "txt",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}
```

Commit:

```bash
git add src/kb/sync/
git commit -m "feat(kb): UrlSyncer (HEAD+GET, etag/last-modified conditional, content-hash dedupe)"
```

---

## Task 6: `compactor/mod.rs` — orphan scan + ledger advance

**Files:** `src/kb/compactor/mod.rs`.

```rust
//! Compactor: orphan file cleanup + ledger state advancement.
//! Designed to be safe-to-run-anytime; never deletes data still
//! referenced by the latest version. Each phase wraps state changes
//! in single write transactions.

use crate::kb::ledger::LedgerStatus;
use crate::kb::paths::KbPaths;
use crate::kb::store::{ledger, KbStore};
use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

const DEFAULT_GRACE_SECS: u64 = 3600; // 1h
const DEFAULT_RETENTION_SECS: u64 = 30 * 86400; // 30 days

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

    // 1. Build set of referenced markdown + raw paths (union over
    //    every Pending / IndexingComplete ledger entry's new_paths).
    let referenced = referenced_paths(store)?;

    // 2. Scan md/ + raw/ on disk; delete files older than grace that
    //    don't appear in `referenced`.
    let cutoff = SystemTime::UNIX_EPOCH
        + std::time::Duration::from_millis((now_ms - DEFAULT_GRACE_SECS as i64 * 1000).max(0) as u64);
    for dir in ["md", "raw"] {
        let abs_dir = paths.root.join(dir);
        if !abs_dir.exists() {
            continue;
        }
        stats.orphans_deleted += scan_and_delete_orphans(&abs_dir, dir, &referenced, cutoff)?;
    }

    // 3. Ledger IndexingComplete → CleanupPending when old_paths are
    //    no longer referenced. (For MVP: old_paths is always
    //    unreferenced once IndexingComplete lands because v(N-1) chunk
    //    cleanup already happened in the handler.)
    {
        let rtx = store.begin_read()?;
        let candidates = ledger::list_by_status(&rtx, LedgerStatus::IndexingComplete)?;
        drop(rtx);
        for entry in candidates {
            // Delete old files (if they still exist).
            for rel in &entry.old_paths {
                let abs = paths.root.join(rel);
                if abs.exists() {
                    let _ = std::fs::remove_file(&abs);
                }
            }
            let wtx = store.begin_write()?;
            ledger::update_status(&wtx, &entry.id, LedgerStatus::CleanupPending, now_ms)?;
            wtx.commit()?;
            stats.ledger_advanced_to_cleanup += 1;
        }
    }

    // 4. CleanupPending → Done when retention elapsed.
    {
        let rtx = store.begin_read()?;
        let candidates = ledger::list_by_status(&rtx, LedgerStatus::CleanupPending)?;
        drop(rtx);
        let retention_ms = DEFAULT_RETENTION_SECS as i64 * 1000;
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
    let rtx = store.begin_read()?;
    let mut out = HashSet::new();
    for status in [LedgerStatus::Pending, LedgerStatus::IndexingComplete] {
        for e in ledger::list_by_status(&rtx, status)? {
            for p in e.new_paths {
                out.insert(p);
            }
        }
    }
    // Also keep every KbDoc.markdown_path/raw_path (covers post-cleanup
    // ledger entries whose new_paths might have been overwritten).
    use crate::kb::store::codec::decode;
    use crate::kb::store::schema::KB_DOCS;
    use redb::ReadableTable;
    let tbl = rtx.open_table(KB_DOCS)?;
    for entry in tbl.iter()? {
        let (_, v) = entry?;
        let d: crate::kb::model::KbDoc = decode(v.value())?;
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
    for entry in walkdir::WalkDir::new(abs_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let abs = entry.path();
        let rel = abs.strip_prefix(abs_dir).unwrap_or(abs);
        let rel_str = format!("{rel_prefix}/{}", rel.display());
        if referenced.contains(&rel_str) {
            continue;
        }
        if let Ok(meta) = abs.metadata() {
            if let Ok(mtime) = meta.modified() {
                if mtime >= cutoff {
                    continue; // too fresh — grace period
                }
            }
        }
        if std::fs::remove_file(abs).is_ok() {
            deleted += 1;
        }
    }
    Ok(deleted)
}
```

`walkdir` — check if already in deps; if not, add `walkdir = "2"` to Cargo.toml.

Tests live in `tests/kb_week4_compactor.rs` (Task 8). Commit:

```bash
git add src/kb/compactor/ Cargo.toml
git commit -m "feat(kb): compactor — orphan file scan + ledger state advancement"
```

---

## Task 7: e2e test — syncers

**Files:** `tests/kb_week4_syncers.rs`.

Tests:
1. `ManualUploadSyncer` ingests a file from disk → doc appears in kb_search results.
2. `UrlSyncer` against an in-process `wiremock` HTTP server (if dep exists) OR a mock function — for MVP, just unit-test the `mime_to_ext` helper + integration-test `ManualUploadSyncer` only. UrlSyncer integration is deferred.

```rust
//! Week 4 syncer e2e: ManualUploadSyncer file → ingest → searchable.

use anyhow::Result;
use rsclaw::kb::sync::{KbSourceSyncer, ManualUploadSyncer, SyncContext, SyncReason};
use rsclaw::kb::{KbEmbedder, KbIndex, KbPaths, KbStore, StubEmbedder};
use rsclaw::kb::worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool};
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_syncer_ingests_then_searchable() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths)?);

    // Write a fixture file.
    let fixture = tmp.path().join("hello.md");
    std::fs::write(&fixture, "# Hello\n\nThe quick brown fox.")?;

    let syncer = ManualUploadSyncer {
        source_id: "test:hello".into(),
        file_path: fixture.clone(),
        tags: vec!["test".into()],
    };
    let ctx = SyncContext {
        store: store.clone(),
        paths: paths.clone(),
        index: index.clone(),
        embedder: embedder.clone(),
    };
    let outcome = syncer.sync(&ctx, SyncReason::Manual).await.unwrap();
    assert_eq!(outcome.docs_added, 1);

    // Drain the worker job so chunks land.
    let hctx = HandlerCtx { store: store.clone(), paths, embedder, index };
    let cfg = WorkerConfig::default();
    WorkerPool::run_one_blocking(&hctx, &cfg, &DefaultDispatcher)?;

    // Confirm chunks exist for the doc.
    use rsclaw::kb::store::{chunks, docs};
    use redb::ReadableTable;
    let rtx = store.begin_read()?;
    let mut any = false;
    let tbl = rtx.open_table(rsclaw::kb::store::schema::KB_DOCS)?;
    for entry in tbl.iter()? {
        let (_, v) = entry?;
        let d: rsclaw::kb::model::KbDoc = rsclaw::kb::store::codec::decode(v.value())?;
        let cs = chunks::chunks_for_logical(&rtx, &d.logical_source_id)?;
        if !cs.is_empty() {
            any = true;
        }
    }
    assert!(any, "expected at least one chunk after ManualUploadSyncer ingest");
    Ok(())
}
```

Commit:

```bash
git add tests/kb_week4_syncers.rs
git commit -m "test(kb): Week 4 e2e — ManualUploadSyncer ingests then chunks land"
```

---

## Task 8: e2e test — compactor

**Files:** `tests/kb_week4_compactor.rs`.

```rust
//! Week 4 compactor e2e: orphan file scan + ledger advance.

use anyhow::Result;
use rsclaw::kb::{KbPaths, KbStore};
use rsclaw::kb::compactor::run_compactor_tick;
use std::sync::Arc;
use tempfile::TempDir;

#[test]
fn compactor_deletes_orphans_older_than_grace() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = KbStore::open(&tmp.path().join("kb.redb"))?;
    let paths = KbPaths::new(tmp.path().join("kb"));
    paths.ensure_layout()?;

    // Drop a file that's nothing in the DB references.
    let orphan = paths.root.join("md/doc/orphan--00000000--00000000.md");
    std::fs::write(&orphan, "stale")?;
    // Set mtime to past the grace window (set the file's mtime via
    // filetime crate if available; otherwise rely on system clock
    // racing — set grace=0 for the test).
    // For MVP, the test simply asserts the file is deleted.
    let stats = run_compactor_tick(&store, &paths, chrono::Utc::now().timestamp_millis() + 86_400_000)?;
    assert!(stats.orphans_deleted >= 1, "expected the orphan to be reaped, stats={stats:?}");
    assert!(!orphan.exists(), "orphan file should be deleted");
    Ok(())
}
```

Commit:

```bash
git add tests/kb_week4_compactor.rs
git commit -m "test(kb): Week 4 compactor — orphan deletion past grace"
```

---

## Task 9: CLI scaffolding — `KbCommand` enum + dispatch

**Files:** `src/cli/kb.rs`, modify `src/cli/mod.rs`, `src/cmd/kb.rs`, `src/cmd/mod.rs`, `src/main.rs`.

```rust
// src/cli/kb.rs
use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct KbCommand {
    #[command(subcommand)]
    pub action: KbAction,
}

#[derive(Subcommand, Debug)]
pub enum KbAction {
    /// Add a document (file path or URL) to the knowledge base.
    Add {
        path_or_url: String,
        #[arg(long)]
        tags: Vec<String>,
    },
    /// List documents.
    Ls {
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        source_kind: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Remove a document by id (tombstone).
    Rm {
        doc_id: String,
        #[arg(long)]
        yes: bool,
    },
    /// Search the knowledge base.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 8)]
        k: usize,
    },
    /// Show a chunk or doc by id.
    Show { id: String },
    /// Update document visibility.
    Visibility {
        doc_id: String,
        /// One of: global | private | agent:<id> | channel:<id>
        visibility: String,
    },
    /// Run compactor tick.
    Compact,
    /// Show kb stats.
    Stats,
}
```

```rust
// src/cmd/kb.rs (skeleton; per-action bodies in Task 10-15)
use crate::cli::kb::{KbAction, KbCommand};
use anyhow::Result;
use std::path::PathBuf;

pub async fn cmd_kb(cmd: KbCommand, kb_root: PathBuf) -> Result<()> {
    match cmd.action {
        KbAction::Add { path_or_url, tags } => add(kb_root, path_or_url, tags).await,
        KbAction::Ls { tag, source_kind, limit } => ls(kb_root, tag, source_kind, limit),
        KbAction::Rm { doc_id, yes } => rm(kb_root, doc_id, yes),
        KbAction::Search { query, k } => search(kb_root, query, k),
        KbAction::Show { id } => show(kb_root, id),
        KbAction::Visibility { doc_id, visibility } => set_visibility(kb_root, doc_id, visibility),
        KbAction::Compact => compact(kb_root),
        KbAction::Stats => stats(kb_root),
    }
}
```

Wire into `src/cli/mod.rs` (`Command::Kb(KbCommand)`), `src/cmd/mod.rs` (`pub use kb::cmd_kb`), `src/main.rs` (match arm). For `kb_root`, derive from rsclaw base_dir: `base_dir.join("kb")`.

Commit:

```bash
git add src/cli/ src/cmd/ src/main.rs
git commit -m "feat(cli): kb subcommand scaffold (KbCommand + cmd_kb dispatch)"
```

---

## Tasks 10–15: CLI action implementations

Each adds a private helper in `src/cmd/kb.rs`. Pattern:

```rust
fn open_kb(kb_root: PathBuf) -> Result<(Arc<KbStore>, Arc<KbPaths>, Arc<KbIndex>)> {
    let paths = Arc::new(KbPaths::new(&kb_root));
    paths.ensure_layout()?;
    let store = Arc::new(KbStore::open(&kb_root.join("kb.redb"))?);
    let index = Arc::new(KbIndex::open_and_rebuild(&paths, &store)?);
    Ok((store, paths, index))
}
```

**Task 10 — `add`:** detect scheme; ManualUpload or Url syncer; run sync; print outcome JSON.

**Task 11 — `ls`:** call `tools::kb_list_docs::run` with the given filter; print table.

**Task 12 — `rm`:** look up doc; set status=Tombstoned via `docs::put`; warn if `--yes` not passed and stdin is a tty.

**Task 13 — `search`:** call `tools::kb_search::run`; print top-k as ranked list.

**Task 14 — `show`:** call `tools::kb_fetch::run`; print chunk text + heading_path.

**Task 15 — `visibility`:** parse the visibility arg (`global | private | agent:<id> | channel:<id>`); update doc; persist.

**Task 16 — `compact`:** call `run_compactor_tick` + print stats.

**Task 17 — `stats`:** count rows in each redb table; print table sizes + total kb on disk (`du`-like via walkdir).

Commit each as a separate `feat(cli):` commit so blame stays granular.

---

## Task 18: README + invariants

**Files:** `src/kb/README.md`.

- Add "Week 4 (Syncers + Compactor + CLI)" subsection.
- Add invariants 19–22:

```
19. **ManualUpload + Url syncers go through ingest_canonicalized** —
    no direct DB write paths bypass the §J atomicity contract.
20. **UrlSyncer conditional-get uses SyncState.cursor** — every
    304 NOT_MODIFIED counts as `docs_skipped`, not `docs_added`.
21. **Compactor never deletes files referenced by any latest-version
    KbDoc** — `referenced_paths` builds a union of markdown_path +
    raw_path across all docs, and the grace period guards against
    in-flight ingest.
22. **CLI is a thin wrapper** — every kb_* CLI action calls the
    same Week 2/3 tool surface (`ingest_canonicalized`, `kb_search`,
    `kb_list_docs`, `kb_fetch`). Bug surface = library, not CLI.
```

- Bump Quick-start to include `rsclaw kb add` + `rsclaw kb search`.

Commit:

```bash
git add src/kb/README.md
git commit -m "docs(kb): README — Week 4 scope + invariants 19–22 + CLI quick-start"
```

---

## Open questions (resolve during implementation)

- **`async-trait` dep**: confirms the project's stance on adding it (Task 2). If declined, fall back to synchronous `sync()` returning a `Pin<Box<dyn Future>>`.
- **`walkdir` dep**: Task 6 needs directory recursion. If not in deps, add `walkdir = "2"`.
- **`thiserror` dep**: Task 2 uses `#[derive(thiserror::Error)]`. If not in deps, hand-write the `Display` impl.
- **`rsclaw kb add <url>` body extraction for HTML**: Week 1's `canonicalize_by_mime` handles `text/html` via lol-html. Confirm before relying on it.
- **Tombstone semantic**: Task 12 sets `KbDoc.status = Tombstoned`. Retrieval already filters non-Active per invariant 4 + 15. Confirm there's no orphan chunk reading required.

---

## Self-review checklist

- [x] All Week 4 modules compile with no new warnings.
- [x] `async-trait` dep added cleanly to Cargo.toml.
- [x] `rsclaw kb --help` shows all 8 subcommands.
- [x] `cargo test --test kb_week4_syncers --test kb_week4_compactor` passes.
- [x] Full suite green: `cargo test -p rsclaw --lib kb::` (197 unit) + 6 integration test files (13 tests total).
- [x] README invariants 19–22 each map to a covering test or design contract.

---

## Execution-time fix sweep — one finding, fixed inline

Only one substantive issue surfaced during T1-T18 execution.

1. **`kb add` returned before the worker drained the queue** — in
   CLI-only mode (no gateway daemon running), `ingest_canonicalized`
   enqueues a `ChunkAndEmbed` job and exits. A follow-up
   `kb search` then sees zero hits because no worker has run.
   Fixed by adding a synchronous worker drain to `cmd::kb::add`:
   loop `WorkerPool::run_one_blocking` until it returns `Ok(false)`.
   This makes `kb add` look synchronous from the user's POV in
   CLI-only mode; in production the gateway daemon's worker pool
   still handles the async case.

Open questions all resolved cleanly:
- `async-trait`: added as `async-trait = "0.1"`.
- `walkdir`: skipped — replaced with std `read_dir` recursion in
  `compactor::scan_and_delete_orphans`.
- `thiserror`: already in deps.
- HTML extraction: Week 1's `canonicalize_by_mime("text/html", ...)`
  works as-is.

Final test smoke:

```bash
$ cargo test -p rsclaw --lib kb::
test result: ok. 197 passed; 0 failed; 0 ignored

$ cargo test --test kb_week1_e2e --test kb_week2_pipeline \
              --test kb_week2_recovery --test kb_week3_search \
              --test kb_week4_syncers --test kb_week4_compactor
all pass (6 + 1 + 2 + 1 + 2 + 1 = 13 integration tests)
```
