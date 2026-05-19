# KB MVP Week 2 — Ingest Pipeline + Worker Pool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the redb persistence layer on top of Week 1's foundation — concrete accessors for KbDoc / KbChunk / IngestLedger / Job / SeenItems / SyncState — wire the atomic `ingest_canonicalized()` pipeline (NOOP-check → stage → single redb tx writing doc + ledger + job + seen + version pointer), and ship a tokio-based worker pool that claims `ChunkAndEmbed` jobs and runs them to completion. By end of Week 2, the engineer can call `ingest_canonicalized(...)` on a `CanonicalizedSource`, get a `doc_id` back synchronously, and observe the worker pool asynchronously chunk + embed + persist chunks to redb (with vectors filled). Crash-recovery integration tests verify the system survives mid-pipeline kills.

**Architecture:** Free-function accessors per redb table (composable inside a single `WriteTransaction`); a `KbStore` facade that owns the `redb::Database` handle and exposes `begin_write`/`begin_read`. The pipeline is a single function that opens one wtx, calls each accessor in order, and commits — atomicity by construction. The worker pool is a tokio task that polls `kb_jobs_by_status_priority` for Ready jobs, claims them atomically (single wtx changes status to Running + writes ClaimToken), dispatches to a `JobHandler` trait impl, and marks Done/Failed on completion. A background reclaim task scans for expired claims and resets them to Ready. Embedder is behind a `KbEmbedder` trait; Week 2 ships a `StubEmbedder` (deterministic sha256-derived vectors) so the pipeline doesn't block on BGE-M3 model integration. **Tantivy and HNSW writes are deferred to Week 3** — Week 2's `chunk_embed` handler writes vectors into the `KbChunk.vector` redb field only.

**Tech Stack:** Rust 2024, tokio (existing), redb 2.x (existing), serde_json (existing), tracing (existing), tempfile (dev, existing), ulid (existing). **No new Cargo deps required.**

**Spec reference:** `docs/specs/2026-05-19-knowledge-base.md` (§J IngestLedger + Outbox, §1 storage map, §2 ingest pipeline).

**Builds on:** `docs/plans/2026-05-19-kb-mvp-week1-foundation.md` (Week 1 — types + content_store + canonicalize + chunker + redb schema).

---

## What this plan delivers

By end of Week 2, the engineer can run:

```bash
cargo test -p rsclaw --lib kb::
cargo test --test kb_week2_pipeline
```

…and have the full integration test pass: given a canonicalized file (re-using Week 1's `canonicalize_by_mime`), the system:

1. Calls `ingest_canonicalized(...)` → returns `doc_id` synchronously
2. After commit: `KbDoc` is in `kb_docs`, `IngestLedgerEntry` is in `kb_ledger` with `Pending` status, `Job` is in `kb_jobs_by_id` with `Ready` status, `kb_doc_latest_version` points at the new doc, `kb_seen_items` records the raw_sha256
3. Worker pool (running in the background) picks up the job within ~100ms, claims it, chunks the markdown, embeds via `StubEmbedder`, writes `KbChunk`s with vectors to `kb_chunks` + `kb_chunk_by_logical`, updates ledger status to `IndexingComplete`, marks job `Done`
4. A second call to `ingest_canonicalized(...)` with identical bytes is a NOOP (returns the existing doc_id, doesn't touch any table)
5. Re-ingesting the same logical source with **different** bytes bumps `KbDoc.version`, enqueues a new job, and stages the new markdown file at a different `lsid8` path (since markdown_sha256 differs → logical_source_id differs)

Crash-recovery integration tests cover:
- Worker process dies mid-chunking → restart → reclaim_stale resets the Running job to Ready → new worker picks it up → chunks are written exactly once (idempotent via deterministic `chunk_id`)
- Drop the `KbStore` handle mid-pipeline (simulates panic between `stage_doc` and `wtx.commit()`) → on restart, orphan markdown file is on disk but no DB record → Week 4 compactor will clean (Week 2 just documents the recovery contract; no compactor yet)

---

## Module additions

```
src/kb/
  store/
    mod.rs              # KbStore facade (exposes db handle + begin_write/begin_read)
    schema.rs           # (Week 1) table definitions
    codec.rs            # NEW: encode/decode helpers (JSON via serde_json)
    docs.rs             # NEW: KbDoc accessors + version helpers
    chunks.rs           # NEW: KbChunk accessors + by_logical scan
    seen.rs             # NEW: SeenItems mark/check + SyncState
    ledger.rs           # NEW: IngestLedger CRUD + list_by_status + update_status
    jobs.rs             # NEW: queue ops (enqueue/claim_next/mark_done/mark_failed/reclaim_stale)
  embedder/
    mod.rs              # NEW: KbEmbedder trait + EmbedderId type
    stub.rs             # NEW: StubEmbedder (deterministic, no model)
  pipeline/
    mod.rs              # NEW: ingest_canonicalized() public API
    ingest.rs           # NEW: pipeline impl (NOOP check, stage, single tx)
  worker/
    mod.rs              # NEW: WorkerPool, start/stop, reclaim task
    pool.rs             # NEW: tokio task implementing the claim loop
    handlers/
      mod.rs            # NEW: JobHandler trait + dispatch by JobKind
      chunk_embed.rs    # NEW: ChunkAndEmbed handler
tests/
  kb_week2_pipeline.rs  # NEW: e2e test through ingest → worker → chunks
  kb_week2_recovery.rs  # NEW: crash recovery scenarios
```

Existing modules (Week 1) stay untouched except `src/kb/mod.rs` gains re-exports for the new public API.

---

## Conventions

- **One commit per task** with `feat(kb): ...` / `test(kb): ...` / `chore(kb): ...`
- **Test placement**: unit tests in `#[cfg(test)] mod tests` at end of source file; integration tests at `tests/kb_week2_*.rs`
- **`cargo test -p rsclaw --lib kb::...`** for unit; `cargo test --test kb_week2_pipeline` / `kb_week2_recovery` for integration
- **No `unwrap()` / `expect()` in non-test code** — use `anyhow::Result`
- **No `println!()` in non-test code** — use `tracing::` macros (NOT `log::`); content goes through `kb::redact()`
- **All public types** `Serialize + Deserialize` where applicable
- **All accessors that write take `&WriteTransaction`** (or `&mut Table<'_>` for hot loops); accessors that read take `&ReadTransaction`. Composing multiple writes in one tx is the entire point of this design.
- **Idempotent handlers**: `chunk_embed` MUST produce the same chunks (same chunk_ids, same vectors) when run twice — the worker reclaim path depends on this

---

## Task 1: Bootstrap — kb module additions

**Files:** Modify `src/kb/mod.rs`; create stubs for new directories.

- [ ] **Step 1: Create new directories + empty mod.rs files**

```bash
mkdir -p src/kb/{embedder,pipeline,worker/handlers}
for d in embedder pipeline worker worker/handlers; do
  : > "src/kb/$d/mod.rs"
done
```

- [ ] **Step 2: Declare new modules in `src/kb/mod.rs`**

Add the following lines to the existing module list:

```rust
pub mod embedder;
pub mod pipeline;
pub mod worker;
```

(Place them alphabetically among the existing `pub mod` lines.)

- [ ] **Step 3: Verify compile**

```bash
cargo check -p rsclaw
```

Expected: passes (empty modules are fine).

- [ ] **Step 4: Commit**

```bash
git add src/kb/mod.rs src/kb/embedder/ src/kb/pipeline/ src/kb/worker/
git commit -m "chore(kb): bootstrap Week 2 module skeleton (embedder, pipeline, worker)"
```

---

## Task 2: `store/codec.rs` — JSON encode/decode helpers

**Files:** `src/kb/store/codec.rs`, modify `src/kb/store/mod.rs`

Every value table stores `serde_json::to_vec(...)` bytes. Centralise the encode/decode so we can swap to a compact binary format in v2 without touching every accessor.

- [ ] **Step 1: Write impl + tests**

```rust
//! Encode/decode helpers for redb value bytes. All Week 2 accessors
//! go through these so v2 can swap JSON for a compact binary codec
//! without touching every table accessor.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).context("kb codec: encode")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).context("kb codec: decode")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct X { a: u32, b: String }

    #[test]
    fn roundtrip() {
        let x = X { a: 7, b: "hi".into() };
        let bytes = encode(&x).unwrap();
        assert_eq!(decode::<X>(&bytes).unwrap(), x);
    }

    #[test]
    fn decode_corrupt_errors() {
        assert!(decode::<X>(b"not json").is_err());
    }
}
```

Update `src/kb/store/mod.rs`:

```rust
pub mod schema;
pub mod codec;
pub use schema::open_db;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::store::codec
git add src/kb/store/
git commit -m "feat(kb): store::codec encode/decode helpers (JSON, swappable in v2)"
```

---

## Task 3: `store/docs.rs` — KbDoc accessors + version helpers

**Files:** `src/kb/store/docs.rs`, modify `src/kb/store/mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
//! KbDoc accessors. Free functions that take a `&WriteTransaction`
//! (writes) or `&ReadTransaction` (reads) so the ingest pipeline can
//! compose multiple table writes in one transaction.

use crate::kb::model::{KbDoc, VersionPointer};
use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::{KB_DOCS, KB_DOC_LATEST_VERSION};
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

/// Insert or replace a doc. The caller is responsible for setting
/// `version` correctly (use `next_version_for` before calling).
pub fn put(wtx: &WriteTransaction, doc: &KbDoc) -> Result<()> {
    let bytes = encode(doc)?;
    let mut tbl = wtx.open_table(KB_DOCS)?;
    tbl.insert(doc.id.as_str(), bytes.as_slice())?;
    Ok(())
}

pub fn get(rtx: &ReadTransaction, doc_id: &str) -> Result<Option<KbDoc>> {
    let tbl = rtx.open_table(KB_DOCS)?;
    match tbl.get(doc_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Set the active-version pointer for a logical source. Called inside
/// the same tx as `put` so re-ingest's version bump is atomic.
pub fn set_latest_version(
    wtx: &WriteTransaction,
    logical_source_id: &str,
    pointer: &VersionPointer,
) -> Result<()> {
    let bytes = encode(pointer)?;
    let mut tbl = wtx.open_table(KB_DOC_LATEST_VERSION)?;
    tbl.insert(logical_source_id, bytes.as_slice())?;
    Ok(())
}

pub fn latest_version(
    rtx: &ReadTransaction,
    logical_source_id: &str,
) -> Result<Option<VersionPointer>> {
    let tbl = rtx.open_table(KB_DOC_LATEST_VERSION)?;
    match tbl.get(logical_source_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Compute the next `KbDoc.version` for a logical source. Returns 1
/// for first ingest, `prev.version + 1` for re-ingest. Reads only.
pub fn next_version_for(rtx: &ReadTransaction, logical_source_id: &str) -> Result<u32> {
    Ok(latest_version(rtx, logical_source_id)?
        .map(|p| p.version + 1)
        .unwrap_or(1))
}

/// Find the latest doc for a logical_source_id whose `raw_sha256`
/// matches `expected_raw_sha`. Used by the NOOP short-circuit in the
/// ingest pipeline. Returns `None` if no match (caller proceeds with
/// fresh ingest).
pub fn find_by_logical_and_hash(
    rtx: &ReadTransaction,
    logical_source_id: &str,
    expected_raw_sha: &str,
) -> Result<Option<String>> {
    let Some(ptr) = latest_version(rtx, logical_source_id)? else {
        return Ok(None);
    };
    let Some(doc) = get(rtx, &ptr.doc_id)? else {
        return Ok(None);
    };
    if doc.raw_sha256 == expected_raw_sha {
        Ok(Some(doc.id))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::model::{KbSource, KbSourceKind, KbStatus, KbVisibility};
    use crate::kb::store::open_db;
    use tempfile::TempDir;

    fn sample(id: &str, lsid: &str, raw_sha: &str, version: u32) -> KbDoc {
        KbDoc {
            id: id.into(),
            logical_source_id: lsid.into(),
            source: KbSource::Doc { path: "/x".into() },
            source_kind: KbSourceKind::Doc,
            title: "T".into(),
            mime: "text/markdown".into(),
            raw_sha256: raw_sha.into(),
            markdown_path: "md/doc/x--12345678.md".into(),
            markdown_sha256: "md".into(),
            raw_path: None,
            owner_user_id: None,
            created_at: 0, updated_at: 0, version,
            status: KbStatus::Active,
            visibility: KbVisibility::Global,
            tags: vec![],
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn put_then_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("d1", "lsid1", "rawA", 1)).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let got = get(&rtx, "d1").unwrap().unwrap();
        assert_eq!(got.raw_sha256, "rawA");
    }

    #[test]
    fn missing_doc_returns_none() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let rtx = db.begin_read().unwrap();
        assert!(get(&rtx, "nope").unwrap().is_none());
    }

    #[test]
    fn next_version_starts_at_1() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let rtx = db.begin_read().unwrap();
        assert_eq!(next_version_for(&rtx, "fresh-lsid").unwrap(), 1);
    }

    #[test]
    fn next_version_increments_after_pointer_set() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            set_latest_version(&wtx, "lsid", &VersionPointer { doc_id: "d1".into(), version: 1 }).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(next_version_for(&rtx, "lsid").unwrap(), 2);
    }

    #[test]
    fn find_by_logical_and_hash_matches() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("d1", "lsid", "rawA", 1)).unwrap();
            set_latest_version(&wtx, "lsid", &VersionPointer { doc_id: "d1".into(), version: 1 }).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(find_by_logical_and_hash(&rtx, "lsid", "rawA").unwrap().as_deref(), Some("d1"));
        assert!(find_by_logical_and_hash(&rtx, "lsid", "rawB").unwrap().is_none());
        assert!(find_by_logical_and_hash(&rtx, "other", "rawA").unwrap().is_none());
    }
}
```

Update `src/kb/store/mod.rs`:

```rust
pub mod schema;
pub mod codec;
pub mod docs;
pub use schema::open_db;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::store::docs
git add src/kb/store/
git commit -m "feat(kb): store::docs accessors (put/get/latest_version/next_version_for/find_by_logical_and_hash)"
```

---

## Task 4: `store/chunks.rs` — KbChunk accessors + by-logical scan

**Files:** `src/kb/store/chunks.rs`, modify `src/kb/store/mod.rs`

The `kb_chunk_by_logical` table is a secondary index keyed by `{logical_source_id}\0{chunk_id}` so re-ingest can find all chunks for a source without scanning `kb_chunks` end-to-end.

- [ ] **Step 1: Write impl + tests**

```rust
//! KbChunk accessors + secondary index by logical_source_id.

use crate::kb::model::KbChunk;
use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::{KB_CHUNKS, KB_CHUNK_BY_LOGICAL};
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

const SEP: u8 = 0;

/// Insert or replace a chunk, updating the by_logical secondary index
/// in the same tx so callers don't have to remember.
pub fn put(wtx: &WriteTransaction, chunk: &KbChunk) -> Result<()> {
    let bytes = encode(chunk)?;
    {
        let mut tbl = wtx.open_table(KB_CHUNKS)?;
        tbl.insert(chunk.id.as_str(), bytes.as_slice())?;
    }
    {
        let mut idx = wtx.open_table(KB_CHUNK_BY_LOGICAL)?;
        let key = compose_logical_key(&chunk.logical_source_id, &chunk.id);
        idx.insert(key.as_str(), b"".as_slice())?;
    }
    Ok(())
}

pub fn get(rtx: &ReadTransaction, chunk_id: &str) -> Result<Option<KbChunk>> {
    let tbl = rtx.open_table(KB_CHUNKS)?;
    match tbl.get(chunk_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Return all `chunk_id`s for a logical source, in `chunk_id` order.
pub fn chunk_ids_for_logical(
    rtx: &ReadTransaction,
    logical_source_id: &str,
) -> Result<Vec<String>> {
    let prefix = format!("{logical_source_id}\0");
    let end = format!("{logical_source_id}\u{1}"); // 0x00 + 1 = 0x01
    let idx = rtx.open_table(KB_CHUNK_BY_LOGICAL)?;
    let mut out = Vec::new();
    for entry in idx.range(prefix.as_str()..end.as_str())? {
        let (k, _) = entry?;
        let key = k.value();
        // key = "{lsid}\0{chunk_id}"
        if let Some(pos) = key.bytes().position(|b| b == SEP) {
            out.push(key[pos + 1..].to_string());
        }
    }
    Ok(out)
}

/// All chunks for a logical source, materialised. Convenience for tests
/// + small docs; for large docs prefer `chunk_ids_for_logical` + per-id
/// `get` so the caller can stream.
pub fn chunks_for_logical(
    rtx: &ReadTransaction,
    logical_source_id: &str,
) -> Result<Vec<KbChunk>> {
    let ids = chunk_ids_for_logical(rtx, logical_source_id)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(c) = get(rtx, &id)? {
            out.push(c);
        }
    }
    Ok(out)
}

fn compose_logical_key(logical_source_id: &str, chunk_id: &str) -> String {
    format!("{logical_source_id}\0{chunk_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::model::{chunk_id, ChunkStatus, KbLocator, LogicalSourceId};
    use crate::kb::store::open_db;
    use tempfile::TempDir;

    fn sample(lsid: &LogicalSourceId, seq: u32, body: &str) -> KbChunk {
        KbChunk {
            id: chunk_id(lsid, seq, body),
            doc_id: "d1".into(),
            logical_source_id: lsid.0.clone(),
            doc_version: 1,
            seq,
            heading_path: vec![],
            byte_offset: (0, body.len() as u64),
            indexed_text: body.into(),
            vector: vec![0.1, 0.2, 0.3],
            simhash: 0,
            locator: KbLocator::Offset { start: 0, end: body.len() },
            status: ChunkStatus::Active,
            source_quality: 1.0,
            embedder_id: "stub".into(),
        }
    }

    #[test]
    fn put_then_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let lsid = LogicalSourceId::for_file("abc");
        let c = sample(&lsid, 0, "hello");
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &c).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(get(&rtx, &c.id).unwrap().unwrap(), c);
    }

    #[test]
    fn chunks_for_logical_returns_in_order() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let lsid = LogicalSourceId::for_file("abc");
        let chunks = (0..3).map(|i| sample(&lsid, i, &format!("body{i}"))).collect::<Vec<_>>();
        {
            let wtx = db.begin_write().unwrap();
            for c in &chunks {
                put(&wtx, c).unwrap();
            }
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let got = chunks_for_logical(&rtx, lsid.as_str()).unwrap();
        assert_eq!(got.len(), 3);
        // chunk_id space is unordered by seq, but every returned chunk
        // belongs to the right logical source.
        for c in &got {
            assert_eq!(c.logical_source_id, lsid.0);
        }
    }

    #[test]
    fn chunks_for_logical_isolates_by_source() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let l1 = LogicalSourceId::for_file("abc");
        let l2 = LogicalSourceId::for_file("def");
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample(&l1, 0, "a")).unwrap();
            put(&wtx, &sample(&l2, 0, "b")).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(chunks_for_logical(&rtx, l1.as_str()).unwrap().len(), 1);
        assert_eq!(chunks_for_logical(&rtx, l2.as_str()).unwrap().len(), 1);
    }

    #[test]
    fn put_overwrites_same_chunk_id() {
        // Same logical+seq+body → same chunk_id → put replaces, doesn't
        // duplicate the by_logical entry.
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let lsid = LogicalSourceId::for_file("abc");
        let c = sample(&lsid, 0, "x");
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &c).unwrap();
            put(&wtx, &c).unwrap(); // re-put same chunk
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(chunks_for_logical(&rtx, lsid.as_str()).unwrap().len(), 1);
    }
}
```

Update `src/kb/store/mod.rs`:

```rust
pub mod chunks;
```

(Plus existing modules.)

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::store::chunks
git add src/kb/store/
git commit -m "feat(kb): store::chunks accessors + by_logical secondary index"
```

---

## Task 5: `store/seen.rs` — SeenItems + SyncState

**Files:** `src/kb/store/seen.rs`, modify `src/kb/store/mod.rs`

`SeenItems` is the cross-syncer dedup table: `(source_id, item_id) → SeenRecord` so the same physical item never gets ingested twice even if multiple syncers race. Week 2 only needs `mark_seen` / `is_seen` from the ingest pipeline. `SyncState` is per-syncer cursor storage; ship the accessors now so Week 4's syncers don't need to add them.

- [ ] **Step 1: Write impl + tests**

```rust
//! SeenItems + SyncState accessors. See spec §S.

use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::{KB_SEEN_ITEMS, KB_SYNC_STATE};
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SeenRecord {
    /// sha256 of the raw bytes the syncer saw. Lets us short-circuit
    /// re-ingest of unchanged items even when their item_id is reused
    /// (e.g. a webhook re-delivery).
    pub raw_sha256: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    /// Opaque per-syncer cursor (e.g. `"etag:abc"`, `"lastmod:..."`).
    pub cursor: String,
    pub last_sync_at: i64,
}

const SEP: char = '\0';

pub fn mark_seen(
    wtx: &WriteTransaction,
    source_id: &str,
    item_id: &str,
    raw_sha256: &str,
    now_ms: i64,
) -> Result<()> {
    let key = compose_seen_key(source_id, item_id);
    let mut tbl = wtx.open_table(KB_SEEN_ITEMS)?;
    let existing = tbl.get(key.as_str())?;
    let rec = match existing {
        Some(v) => {
            let mut r: SeenRecord = decode(v.value())?;
            r.last_seen_at = now_ms;
            r.raw_sha256 = raw_sha256.into();
            r
        }
        None => SeenRecord {
            raw_sha256: raw_sha256.into(),
            first_seen_at: now_ms,
            last_seen_at: now_ms,
        },
    };
    let bytes = encode(&rec)?;
    tbl.insert(key.as_str(), bytes.as_slice())?;
    Ok(())
}

pub fn is_seen(
    rtx: &ReadTransaction,
    source_id: &str,
    item_id: &str,
) -> Result<Option<SeenRecord>> {
    let key = compose_seen_key(source_id, item_id);
    let tbl = rtx.open_table(KB_SEEN_ITEMS)?;
    match tbl.get(key.as_str())? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

fn compose_seen_key(source_id: &str, item_id: &str) -> String {
    format!("{source_id}{SEP}{item_id}")
}

pub fn put_sync_state(
    wtx: &WriteTransaction,
    source_id: &str,
    state: &SyncState,
) -> Result<()> {
    let bytes = encode(state)?;
    let mut tbl = wtx.open_table(KB_SYNC_STATE)?;
    tbl.insert(source_id, bytes.as_slice())?;
    Ok(())
}

pub fn get_sync_state(rtx: &ReadTransaction, source_id: &str) -> Result<Option<SyncState>> {
    let tbl = rtx.open_table(KB_SYNC_STATE)?;
    match tbl.get(source_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::store::open_db;
    use tempfile::TempDir;

    #[test]
    fn mark_then_query() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            mark_seen(&wtx, "src1", "item1", "sha-A", 1000).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let r = is_seen(&rtx, "src1", "item1").unwrap().unwrap();
        assert_eq!(r.raw_sha256, "sha-A");
        assert_eq!(r.first_seen_at, 1000);
        assert_eq!(r.last_seen_at, 1000);
    }

    #[test]
    fn mark_twice_updates_last_seen() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            mark_seen(&wtx, "src1", "item1", "sha-A", 1000).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            mark_seen(&wtx, "src1", "item1", "sha-B", 2000).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let r = is_seen(&rtx, "src1", "item1").unwrap().unwrap();
        assert_eq!(r.first_seen_at, 1000); // preserved
        assert_eq!(r.last_seen_at, 2000);  // updated
        assert_eq!(r.raw_sha256, "sha-B"); // updated
    }

    #[test]
    fn keys_isolate_by_source() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            mark_seen(&wtx, "src1", "x", "a", 1).unwrap();
            mark_seen(&wtx, "src2", "x", "b", 1).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(is_seen(&rtx, "src1", "x").unwrap().unwrap().raw_sha256, "a");
        assert_eq!(is_seen(&rtx, "src2", "x").unwrap().unwrap().raw_sha256, "b");
    }

    #[test]
    fn sync_state_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put_sync_state(
                &wtx,
                "src1",
                &SyncState { cursor: "etag:abc".into(), last_sync_at: 123 },
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let s = get_sync_state(&rtx, "src1").unwrap().unwrap();
        assert_eq!(s.cursor, "etag:abc");
        assert_eq!(s.last_sync_at, 123);
    }
}
```

Update `src/kb/store/mod.rs`:

```rust
pub mod seen;
pub use seen::{SeenRecord, SyncState};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::store::seen
git add src/kb/store/
git commit -m "feat(kb): store::seen mark_seen/is_seen + SyncState accessors"
```

---

## Task 6: `store/ledger.rs` — IngestLedger CRUD + status helpers

**Files:** `src/kb/store/ledger.rs`, modify `src/kb/store/mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
//! IngestLedger accessors. See spec §J.

use crate::kb::ledger::{IngestLedgerEntry, LedgerStatus};
use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::KB_LEDGER;
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

pub fn put(wtx: &WriteTransaction, entry: &IngestLedgerEntry) -> Result<()> {
    let bytes = encode(entry)?;
    let mut tbl = wtx.open_table(KB_LEDGER)?;
    tbl.insert(entry.id.as_str(), bytes.as_slice())?;
    Ok(())
}

pub fn get(rtx: &ReadTransaction, ledger_id: &str) -> Result<Option<IngestLedgerEntry>> {
    let tbl = rtx.open_table(KB_LEDGER)?;
    match tbl.get(ledger_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Scan the entire ledger table and return entries matching `status`.
/// Week 2: linear scan is fine — ledger is small (1 entry per ingest).
/// Week 4 compactor adds a status-indexed table if scan becomes hot.
pub fn list_by_status(
    rtx: &ReadTransaction,
    status: LedgerStatus,
) -> Result<Vec<IngestLedgerEntry>> {
    let tbl = rtx.open_table(KB_LEDGER)?;
    let mut out = Vec::new();
    for entry in tbl.iter()? {
        let (_, v) = entry?;
        let e: IngestLedgerEntry = decode(v.value())?;
        if e.status == status {
            out.push(e);
        }
    }
    Ok(out)
}

/// Update an existing ledger entry's status + updated_at. Errors if
/// the entry doesn't exist (callers shouldn't be transitioning
/// non-existent ledgers).
pub fn update_status(
    wtx: &WriteTransaction,
    ledger_id: &str,
    new_status: LedgerStatus,
    now_ms: i64,
) -> Result<()> {
    let mut tbl = wtx.open_table(KB_LEDGER)?;
    let v = tbl
        .get(ledger_id)?
        .ok_or_else(|| anyhow::anyhow!("ledger {ledger_id} not found"))?;
    let mut entry: IngestLedgerEntry = decode(v.value())?;
    drop(v);
    entry.status = new_status;
    entry.updated_at = now_ms;
    let bytes = encode(&entry)?;
    tbl.insert(ledger_id, bytes.as_slice())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::ledger::{LedgerOp, LedgerStatus};
    use crate::kb::store::open_db;
    use tempfile::TempDir;

    fn sample(id: &str, status: LedgerStatus) -> IngestLedgerEntry {
        IngestLedgerEntry {
            id: id.into(),
            created_at: 0, updated_at: 0,
            doc_id: "d1".into(),
            logical_source_id: "lsid".into(),
            op: LedgerOp::Create,
            new_paths: vec![],
            old_paths: vec![],
            status,
            error: None,
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("L1", LedgerStatus::Pending)).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(get(&rtx, "L1").unwrap().unwrap().status, LedgerStatus::Pending);
    }

    #[test]
    fn list_by_status_filters_correctly() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("L1", LedgerStatus::Pending)).unwrap();
            put(&wtx, &sample("L2", LedgerStatus::Pending)).unwrap();
            put(&wtx, &sample("L3", LedgerStatus::Done)).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let pending = list_by_status(&rtx, LedgerStatus::Pending).unwrap();
        assert_eq!(pending.len(), 2);
        let done = list_by_status(&rtx, LedgerStatus::Done).unwrap();
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn update_status_changes_state() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("L1", LedgerStatus::Pending)).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            update_status(&wtx, "L1", LedgerStatus::IndexingComplete, 999).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let e = get(&rtx, "L1").unwrap().unwrap();
        assert_eq!(e.status, LedgerStatus::IndexingComplete);
        assert_eq!(e.updated_at, 999);
    }

    #[test]
    fn update_status_errors_when_missing() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let wtx = db.begin_write().unwrap();
        assert!(update_status(&wtx, "nope", LedgerStatus::Done, 0).is_err());
    }
}
```

Update `src/kb/store/mod.rs`:

```rust
pub mod ledger;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::store::ledger
git add src/kb/store/
git commit -m "feat(kb): store::ledger put/get/list_by_status/update_status"
```

---

## Task 7: `store/jobs.rs` — Job queue (enqueue / claim_next / mark_done / mark_failed / reclaim_stale)

**Files:** `src/kb/store/jobs.rs`, modify `src/kb/store/mod.rs`

This is the centrepiece of Week 2. Four redb tables form the queue:
- `kb_jobs_by_id` — job_id → Job
- `kb_jobs_by_dedupe_active` — dedupe_key → job_id (only Ready/Running entries)
- `kb_jobs_by_status_priority` — `{status_byte}{prio_byte}{created_at_be}{job_id}` → () — range scan finds next job
- `kb_job_claims` — job_id → ClaimToken

All transitions are single-tx and update the index tables together.

- [ ] **Step 1: Write impl + tests**

```rust
//! Job queue accessors. The queue is redb-native: four tables tracked
//! atomically via single write transactions. See spec §J.
//!
//! Layout of the priority key: `{status_byte}{prio_byte}{created_at_be}{job_id}`.
//! Lower byte values sort first → Ready=0 before Running=1; lower
//! priority byte = higher actual priority. created_at big-endian
//! makes older jobs sort before newer at same priority. job_id
//! disambiguates the tail.

use crate::kb::jobs::{status_priority_key, ClaimToken, Job, JobStatus};
use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::{
    KB_JOBS_BY_DEDUPE_ACTIVE, KB_JOBS_BY_ID, KB_JOBS_BY_STATUS_PRIO, KB_JOB_CLAIMS,
};
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

/// Enqueue a new job. Idempotent on `dedupe_key`: if an active
/// (Ready/Running) job already exists with the same dedupe_key, this
/// is a no-op and returns the existing job_id.
///
/// Returns the id of the job that's now in the queue (either the new
/// one or the pre-existing duplicate).
pub fn enqueue(wtx: &WriteTransaction, job: &Job) -> Result<String> {
    let dedupe_key = job.kind.dedupe_key();
    {
        let dedupe = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
        if let Some(existing) = dedupe.get(dedupe_key.as_str())? {
            return Ok(existing.value().to_string());
        }
    }
    // No duplicate active job → write all four index entries.
    {
        let mut by_id = wtx.open_table(KB_JOBS_BY_ID)?;
        let bytes = encode(job)?;
        by_id.insert(job.id.as_str(), bytes.as_slice())?;
    }
    {
        let mut dedupe = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
        dedupe.insert(dedupe_key.as_str(), job.id.as_str())?;
    }
    {
        let mut prio = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
        prio.insert(status_priority_key(job).as_slice(), b"".as_slice())?;
    }
    Ok(job.id.clone())
}

/// Find the highest-priority Ready job and atomically claim it
/// (change status to Running, write claim token). Returns the job +
/// claim token; returns `None` if no Ready jobs exist.
pub fn claim_next(
    wtx: &WriteTransaction,
    worker_id: &str,
    now_ms: i64,
    claim_ttl_ms: i64,
) -> Result<Option<(Job, ClaimToken)>> {
    // Find first key with status=Ready (status_byte=0). Range from
    // [0x00...] to [0x01...] gives all Ready jobs in priority order.
    let lo: &[u8] = &[JobStatus::Ready.as_byte()];
    let hi: &[u8] = &[JobStatus::Ready.as_byte() + 1];
    let prio = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
    let mut iter = prio.range::<&[u8]>(lo..hi)?;
    let Some(first) = iter.next() else {
        return Ok(None);
    };
    let (k, _) = first?;
    let old_key = k.value().to_vec();
    drop(iter);

    let job_id = job_id_from_priority_key(&old_key);

    // Read the job, change status, write back, update indices.
    let job = {
        let by_id = wtx.open_table(KB_JOBS_BY_ID)?;
        let v = by_id
            .get(job_id.as_str())?
            .ok_or_else(|| anyhow::anyhow!("priority index points at missing job {job_id}"))?;
        decode::<Job>(v.value())?
    };

    let mut new_job = job.clone();
    new_job.status = JobStatus::Running;

    {
        let mut by_id = wtx.open_table(KB_JOBS_BY_ID)?;
        by_id.insert(new_job.id.as_str(), encode(&new_job)?.as_slice())?;
    }
    {
        let mut prio = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
        prio.remove(old_key.as_slice())?;
        prio.insert(status_priority_key(&new_job).as_slice(), b"".as_slice())?;
    }
    let token = ClaimToken {
        worker_id: worker_id.into(),
        claimed_at: now_ms,
        expires_at: now_ms + claim_ttl_ms,
        token: ulid::Ulid::new().to_string(),
    };
    {
        let mut claims = wtx.open_table(KB_JOB_CLAIMS)?;
        claims.insert(new_job.id.as_str(), encode(&token)?.as_slice())?;
    }

    Ok(Some((new_job, token)))
}

/// Mark a job Done. Removes it from `kb_jobs_by_dedupe_active`,
/// removes the claim token, and updates the priority index to reflect
/// the new status. The job row stays in `kb_jobs_by_id` for audit /
/// retry history.
pub fn mark_done(wtx: &WriteTransaction, job_id: &str) -> Result<()> {
    let (mut job, old_key) = read_and_old_key(wtx, job_id)?;
    let dedupe_key = job.kind.dedupe_key();
    job.status = JobStatus::Done;
    write_status_transition(wtx, &job, &old_key)?;
    {
        let mut dedupe = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
        dedupe.remove(dedupe_key.as_str())?;
    }
    {
        let mut claims = wtx.open_table(KB_JOB_CLAIMS)?;
        claims.remove(job_id)?;
    }
    Ok(())
}

/// Mark a job Failed. Same shape as mark_done — removes from
/// dedupe + claims, but keeps the row for visibility (UI can list
/// Failed for human triage).
pub fn mark_failed(wtx: &WriteTransaction, job_id: &str, error: &str) -> Result<()> {
    let (mut job, old_key) = read_and_old_key(wtx, job_id)?;
    let dedupe_key = job.kind.dedupe_key();
    job.status = JobStatus::Failed;
    job.attempts += 1;
    job.last_error = Some(error.into());
    write_status_transition(wtx, &job, &old_key)?;
    {
        let mut dedupe = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
        dedupe.remove(dedupe_key.as_str())?;
    }
    {
        let mut claims = wtx.open_table(KB_JOB_CLAIMS)?;
        claims.remove(job_id)?;
    }
    Ok(())
}

/// Reset a job back to Ready (e.g. after a recoverable error). Keeps
/// the dedupe entry (job is still active). Increments attempts. Used
/// by reclaim_stale and by handler retry policy.
pub fn requeue(wtx: &WriteTransaction, job_id: &str) -> Result<()> {
    let (mut job, old_key) = read_and_old_key(wtx, job_id)?;
    job.status = JobStatus::Ready;
    job.attempts += 1;
    write_status_transition(wtx, &job, &old_key)?;
    {
        let mut claims = wtx.open_table(KB_JOB_CLAIMS)?;
        claims.remove(job_id)?;
    }
    Ok(())
}

/// Find Running jobs whose claim token has expired (worker died /
/// stalled), reset them to Ready. Returns the ids that were reclaimed.
pub fn reclaim_stale(wtx: &WriteTransaction, now_ms: i64) -> Result<Vec<String>> {
    let mut to_reclaim = Vec::new();
    {
        let claims = wtx.open_table(KB_JOB_CLAIMS)?;
        for entry in claims.iter()? {
            let (k, v) = entry?;
            let token: ClaimToken = decode(v.value())?;
            if token.expires_at < now_ms {
                to_reclaim.push(k.value().to_string());
            }
        }
    }
    for id in &to_reclaim {
        requeue(wtx, id)?;
    }
    Ok(to_reclaim)
}

pub fn get(rtx: &ReadTransaction, job_id: &str) -> Result<Option<Job>> {
    let by_id = rtx.open_table(KB_JOBS_BY_ID)?;
    match by_id.get(job_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

pub fn list_by_status(rtx: &ReadTransaction, status: JobStatus) -> Result<Vec<Job>> {
    let lo: &[u8] = &[status.as_byte()];
    let hi: &[u8] = &[status.as_byte() + 1];
    let prio = rtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
    let by_id = rtx.open_table(KB_JOBS_BY_ID)?;
    let mut out = Vec::new();
    for entry in prio.range::<&[u8]>(lo..hi)? {
        let (k, _) = entry?;
        let id = job_id_from_priority_key(k.value());
        if let Some(v) = by_id.get(id.as_str())? {
            out.push(decode(v.value())?);
        }
    }
    Ok(out)
}

// ----- internals -----

fn read_and_old_key(wtx: &WriteTransaction, job_id: &str) -> Result<(Job, Vec<u8>)> {
    let by_id = wtx.open_table(KB_JOBS_BY_ID)?;
    let v = by_id
        .get(job_id)?
        .ok_or_else(|| anyhow::anyhow!("job {job_id} not found"))?;
    let job: Job = decode(v.value())?;
    let old_key = status_priority_key(&job);
    Ok((job, old_key))
}

fn write_status_transition(
    wtx: &WriteTransaction,
    new_job: &Job,
    old_key: &[u8],
) -> Result<()> {
    {
        let mut by_id = wtx.open_table(KB_JOBS_BY_ID)?;
        by_id.insert(new_job.id.as_str(), encode(new_job)?.as_slice())?;
    }
    {
        let mut prio = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
        prio.remove(old_key)?;
        prio.insert(status_priority_key(new_job).as_slice(), b"".as_slice())?;
    }
    Ok(())
}

fn job_id_from_priority_key(key: &[u8]) -> String {
    // key = status_byte(1) + prio_byte(1) + created_at_be(8) + job_id_bytes
    if key.len() <= 10 {
        return String::new();
    }
    String::from_utf8_lossy(&key[10..]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::jobs::{Job, JobKind};
    use crate::kb::store::open_db;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, redb::Database) {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        (tmp, db)
    }

    #[test]
    fn enqueue_then_claim() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::ChunkAndEmbed {
            doc_id: "d1".into(),
            doc_version: 1,
        });
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            assert_eq!(enqueue(&wtx, &job).unwrap(), job_id);
            wtx.commit().unwrap();
        }
        let claimed = {
            let wtx = db.begin_write().unwrap();
            let (j, _t) = claim_next(&wtx, "worker-1", 1000, 60_000).unwrap().unwrap();
            wtx.commit().unwrap();
            j
        };
        assert_eq!(claimed.id, job_id);
        assert_eq!(claimed.status, JobStatus::Running);
    }

    #[test]
    fn enqueue_dedupes_active_jobs() {
        let (_tmp, db) = fresh();
        let kind = JobKind::ChunkAndEmbed {
            doc_id: "d1".into(),
            doc_version: 1,
        };
        let j1 = Job::new(kind.clone());
        let j2 = Job::new(kind);
        let id_first = j1.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            let id_a = enqueue(&wtx, &j1).unwrap();
            let id_b = enqueue(&wtx, &j2).unwrap();
            assert_eq!(id_a, id_first);
            assert_eq!(id_b, id_first, "dedupe must return existing job_id");
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(list_by_status(&rtx, JobStatus::Ready).unwrap().len(), 1);
    }

    #[test]
    fn claim_next_returns_none_when_empty() {
        let (_tmp, db) = fresh();
        let wtx = db.begin_write().unwrap();
        assert!(claim_next(&wtx, "w", 0, 60_000).unwrap().is_none());
    }

    #[test]
    fn mark_done_removes_from_dedupe_and_claims() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RebuildHnsw);
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            claim_next(&wtx, "w", 0, 60_000).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            mark_done(&wtx, &job_id).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        // Job row stays in by_id for audit
        let j = get(&rtx, &job_id).unwrap().unwrap();
        assert_eq!(j.status, JobStatus::Done);
        // But dedupe + claim are gone, so re-enqueue works.
        let new_job = Job::new(JobKind::RebuildHnsw);
        let new_id = new_job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            assert_eq!(enqueue(&wtx, &new_job).unwrap(), new_id);
            wtx.commit().unwrap();
        }
    }

    #[test]
    fn mark_failed_increments_attempts() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RunCompactor);
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            claim_next(&wtx, "w", 0, 60_000).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            mark_failed(&wtx, &job_id, "boom").unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let j = get(&rtx, &job_id).unwrap().unwrap();
        assert_eq!(j.status, JobStatus::Failed);
        assert_eq!(j.attempts, 1);
        assert_eq!(j.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn reclaim_stale_resets_expired_claims() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RebuildHnsw);
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            wtx.commit().unwrap();
        }
        // Claim with short TTL
        {
            let wtx = db.begin_write().unwrap();
            claim_next(&wtx, "w", 100, 50).unwrap(); // expires at 150
            wtx.commit().unwrap();
        }
        // now=200 → claim is stale
        let reclaimed = {
            let wtx = db.begin_write().unwrap();
            let ids = reclaim_stale(&wtx, 200).unwrap();
            wtx.commit().unwrap();
            ids
        };
        assert_eq!(reclaimed, vec![job_id.clone()]);
        let rtx = db.begin_read().unwrap();
        let j = get(&rtx, &job_id).unwrap().unwrap();
        assert_eq!(j.status, JobStatus::Ready);
        assert_eq!(j.attempts, 1);
    }

    #[test]
    fn reclaim_stale_skips_fresh_claims() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RebuildHnsw);
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            claim_next(&wtx, "w", 100, 60_000).unwrap();
            wtx.commit().unwrap();
        }
        let wtx = db.begin_write().unwrap();
        assert!(reclaim_stale(&wtx, 200).unwrap().is_empty());
    }

    #[test]
    fn priority_order_oldest_highest_priority_first() {
        let (_tmp, db) = fresh();
        let mut low = Job::new(JobKind::ChunkAndEmbed { doc_id: "d1".into(), doc_version: 1 });
        low.priority = 200;
        let mut high = Job::new(JobKind::ChunkAndEmbed { doc_id: "d2".into(), doc_version: 1 });
        high.priority = 10;
        let high_id = high.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &low).unwrap();
            enqueue(&wtx, &high).unwrap();
            wtx.commit().unwrap();
        }
        let wtx = db.begin_write().unwrap();
        let (j, _) = claim_next(&wtx, "w", 0, 60_000).unwrap().unwrap();
        assert_eq!(j.id, high_id, "lower priority byte must claim first");
    }
}
```

Update `src/kb/store/mod.rs`:

```rust
pub mod jobs;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::store::jobs
git add src/kb/store/
git commit -m "feat(kb): store::jobs queue ops (enqueue/claim_next/mark_done/mark_failed/reclaim_stale/requeue)"
```

---

## Task 8: `store/mod.rs` — `KbStore` facade

**Files:** modify `src/kb/store/mod.rs`

A thin handle that owns the `redb::Database` and exposes `begin_write` / `begin_read`. The actual logic lives in the per-table modules; this just keeps callers from threading the raw `Database` around.

- [ ] **Step 1: Write facade + tests**

Replace the existing `src/kb/store/mod.rs` contents with:

```rust
//! redb-backed KB store. `KbStore` owns the database handle; all
//! reads/writes go through tx accessors defined in the submodules.

pub mod schema;
pub mod codec;
pub mod docs;
pub mod chunks;
pub mod seen;
pub mod ledger;
pub mod jobs;

pub use schema::open_db;
pub use seen::{SeenRecord, SyncState};

use anyhow::Result;
use redb::{Database, ReadTransaction, WriteTransaction};
use std::path::Path;

pub struct KbStore {
    pub db: Database,
}

impl KbStore {
    pub fn open(path: &Path) -> Result<Self> {
        let db = open_db(path)?;
        Ok(Self { db })
    }

    pub fn begin_write(&self) -> Result<WriteTransaction> {
        Ok(self.db.begin_write()?)
    }

    pub fn begin_read(&self) -> Result<ReadTransaction> {
        Ok(self.db.begin_read()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_creates_db() {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let _rtx = store.begin_read().unwrap();
    }

    #[test]
    fn begin_write_returns_usable_tx() {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let wtx = store.begin_write().unwrap();
        wtx.commit().unwrap();
    }
}
```

- [ ] **Step 2: Add re-export to `src/kb/mod.rs`**

```rust
pub use store::KbStore;
```

(In the existing `pub use store::open_db;` neighbourhood.)

- [ ] **Step 3: Run + commit**

```bash
cargo test -p rsclaw --lib kb::store
git add src/kb/store/ src/kb/mod.rs
git commit -m "feat(kb): KbStore facade (owns redb::Database, exposes begin_write/begin_read)"
```

---

## Task 9: `embedder/` — KbEmbedder trait + StubEmbedder

**Files:** `src/kb/embedder/mod.rs`, `src/kb/embedder/stub.rs`

Week 2 does NOT ship a real BGE-M3 embedder (that lands in Week 2.5 or 3, isolated behind this trait). The pipeline + worker pool need *something* to call so vectors land in `KbChunk.vector` — a deterministic stub is the right answer:

- Deterministic = chunk_id → same vector across runs, makes the worker's idempotency easy to verify
- Pure Rust = no model weights to download, no candle setup cost, no CI flakiness
- Honest dimensions = 1024 (matches BGE-M3) so Week 3's HNSW schema fits without a swap

- [ ] **Step 1: Write trait + stub + tests**

```rust
//! Embedder trait + deterministic stub used in Week 2.
//!
//! The real BGE-M3 embedder (candle-based, ~2GB weights) is a
//! self-contained follow-up that swaps in behind `KbEmbedder` once
//! the pipeline + worker pool are proven correct. Until then,
//! `StubEmbedder` returns sha256-derived deterministic vectors so
//! handler idempotency tests are easy to write.

pub mod stub;

use anyhow::Result;

pub use stub::StubEmbedder;

pub trait KbEmbedder: Send + Sync {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
    fn embedder_id(&self) -> &str;
}
```

```rust
// src/kb/embedder/stub.rs
//! Deterministic stub embedder. Derives each vector from
//! sha256(text), expands to `dimension()` f32 values normalised to
//! the unit hypersphere (so cosine similarity tests are sensible).

use super::KbEmbedder;
use anyhow::Result;
use sha2::{Digest, Sha256};

pub struct StubEmbedder {
    pub dimension: usize,
    pub id: String,
}

impl Default for StubEmbedder {
    fn default() -> Self {
        Self {
            dimension: 1024,
            id: "stub-sha256-1024".into(),
        }
    }
}

impl KbEmbedder for StubEmbedder {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn embedder_id(&self) -> &str {
        &self.id
    }
}

impl StubEmbedder {
    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = Vec::with_capacity(self.dimension);
        // Expand sha256(text) by hashing (text || counter) for each
        // 32-byte block until we have `dimension` f32s. Deterministic
        // and fast.
        let mut block = 0u32;
        while v.len() < self.dimension {
            let mut h = Sha256::new();
            h.update(text.as_bytes());
            h.update(block.to_be_bytes());
            let bytes = h.finalize();
            for chunk in bytes.chunks_exact(4) {
                if v.len() == self.dimension {
                    break;
                }
                let u = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                // map u32 → [-1.0, 1.0)
                v.push((u as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32);
            }
            block += 1;
        }
        // L2-normalise to unit length so cosine = dot product.
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        for x in &mut v {
            *x /= norm;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_default_is_1024() {
        let e = StubEmbedder::default();
        let v = e.embed_batch(&["hi".into()]).unwrap();
        assert_eq!(v[0].len(), 1024);
    }

    #[test]
    fn deterministic() {
        let e = StubEmbedder::default();
        let a = e.embed_batch(&["same".into()]).unwrap();
        let b = e.embed_batch(&["same".into()]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_inputs_different_vectors() {
        let e = StubEmbedder::default();
        let v = e.embed_batch(&["a".into(), "b".into()]).unwrap();
        assert_ne!(v[0], v[1]);
    }

    #[test]
    fn batch_preserves_order() {
        let e = StubEmbedder::default();
        let inputs: Vec<String> = (0..5).map(|i| format!("text {i}")).collect();
        let v = e.embed_batch(&inputs).unwrap();
        assert_eq!(v.len(), 5);
        for (i, t) in inputs.iter().enumerate() {
            let single = e.embed_batch(&[t.clone()]).unwrap();
            assert_eq!(v[i], single[0]);
        }
    }

    #[test]
    fn vectors_are_unit_length() {
        let e = StubEmbedder::default();
        let v = &e.embed_batch(&["test".into()]).unwrap()[0];
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "got norm = {norm}");
    }
}
```

- [ ] **Step 2: Re-export from `src/kb/mod.rs`**

```rust
pub use embedder::{KbEmbedder, StubEmbedder};
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p rsclaw --lib kb::embedder
git add src/kb/embedder/ src/kb/mod.rs
git commit -m "feat(kb): KbEmbedder trait + StubEmbedder (deterministic, 1024-dim)"
```

---

## Task 10: `pipeline/ingest.rs` — `ingest_canonicalized` atomic pipeline

**Files:** `src/kb/pipeline/mod.rs`, `src/kb/pipeline/ingest.rs`

The single function that ties everything together. One redb tx writes the doc + ledger + job + seen + version pointer atomically. Returns the `doc_id` synchronously after commit.

- [ ] **Step 1: Write impl + tests**

```rust
//! ingest_canonicalized: the single atomic step that turns a
//! CanonicalizedSource into a persisted KbDoc + enqueued
//! ChunkAndEmbed job. See spec §J Ingest 流程.
//!
//! Atomicity contract: the function commits one redb write
//! transaction that contains *all* of:
//!   1. KbDoc row (kb_docs)
//!   2. VersionPointer (kb_doc_latest_version)
//!   3. IngestLedgerEntry (kb_ledger)
//!   4. Job + dedupe + priority index (3 jobs tables)
//!   5. SeenItems entry (kb_seen_items)
//!
//! If the tx fails to commit, NONE of these land — but the markdown
//! file on disk (staged before the tx opens) becomes an orphan.
//! The Week 4 compactor's grace-period orphan scan reclaims it.

use crate::kb::canonicalize::CanonicalizedSource;
use crate::kb::content_store::{
    atomic::sha256_hex, compose::FrontMatter, paths::slugify, stage_doc, StageInput,
};
use crate::kb::jobs::{Job, JobKind};
use crate::kb::ledger::{IngestLedgerEntry, LedgerOp, LedgerStatus};
use crate::kb::model::{KbDoc, KbSource, KbStatus, KbVisibility, VersionPointer};
use crate::kb::paths::KbPaths;
use crate::kb::store::{docs, jobs, ledger, seen, KbStore};
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct IngestInput<'a> {
    pub canon: &'a CanonicalizedSource,
    /// Raw bytes that produced `canon` (pre-canonicalize). Used for
    /// `raw_sha256` + optionally staged under `raw/` for re-extract.
    pub raw_bytes: &'a [u8],
    /// File extension for raw staging (e.g. "pdf"). Empty string ok.
    pub raw_ext: &'a str,
    /// Override visibility. `None` → `KbVisibility::default_for(source_kind)`.
    pub visibility: Option<KbVisibility>,
    /// Owner_user_id, set when visibility is `Private`.
    pub owner_user_id: Option<String>,
    /// `(source_id, item_id)` for cross-syncer dedup; if `None`, the
    /// pipeline skips the `kb_seen_items` write. Manual uploads
    /// usually pass `Some(("manual", logical_source_id))`.
    pub seen_key: Option<(&'a str, &'a str)>,
    /// Optional KbSource override (e.g. with the original file path).
    /// Defaults to a placeholder for tests; production callers should
    /// always supply this.
    pub source: Option<KbSource>,
    /// `kb_root` for file staging. Reads from `store.db` are independent.
    pub paths: &'a KbPaths,
}

#[derive(Debug, Clone)]
pub struct IngestOutput {
    /// `KbDoc.id` of the doc that's now visible (either freshly
    /// created or pre-existing if NOOP).
    pub doc_id: String,
    /// `true` when this ingest was a NOOP (logical_source_id +
    /// raw_sha256 already on disk and pointed at by latest_version).
    /// In that case `markdown_rel_path` is the existing file.
    pub noop: bool,
    pub markdown_rel_path: String,
}

pub fn ingest_canonicalized(store: &KbStore, input: IngestInput<'_>) -> Result<IngestOutput> {
    let raw_sha = sha256_hex(input.raw_bytes);
    let lsid_str = input.canon.metadata.logical_source_id.as_str().to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();

    // 1. NOOP short-circuit: identical lsid + raw_sha → return existing doc_id
    {
        let rtx = store.begin_read()?;
        if let Some(existing_doc_id) = docs::find_by_logical_and_hash(&rtx, &lsid_str, &raw_sha)? {
            if let Some(existing) = docs::get(&rtx, &existing_doc_id)? {
                tracing::info!(
                    doc = %crate::kb::redact(&existing_doc_id),
                    "kb ingest: noop"
                );
                return Ok(IngestOutput {
                    doc_id: existing_doc_id,
                    noop: true,
                    markdown_rel_path: existing.markdown_path,
                });
            }
        }
    }

    // 2. Stage markdown + raw bytes on disk (before opening the tx).
    //    If the tx later fails, these files become orphans → Week 4
    //    compactor cleans them.
    let doc_id = ulid::Ulid::new().to_string();
    let slug = slugify(&input.canon.metadata.title);
    let staged = stage_doc(
        input.paths,
        StageInput {
            doc_id: &doc_id,
            kind: input.canon.metadata.source_kind,
            slug: &slug,
            logical_source_id: &lsid_str,
            front: FrontMatter {
                title: input.canon.metadata.title.clone(),
                source_kind: input.canon.metadata.source_kind.as_str().to_string(),
                logical_source_id: lsid_str.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                tags: input.canon.metadata.tags.clone(),
                meta: input.canon.metadata.extra.clone(),
            },
            body: &input.canon.markdown,
            raw: Some((input.raw_bytes, input.raw_ext)),
            keep_raw: true,
        },
    )?;

    // 3. Compute version + old_paths (if this is an Update).
    let rtx = store.begin_read()?;
    let next_version = docs::next_version_for(&rtx, &lsid_str)?;
    let old_paths = if next_version > 1 {
        let ptr = docs::latest_version(&rtx, &lsid_str)?.unwrap();
        match docs::get(&rtx, &ptr.doc_id)? {
            Some(prev) => {
                let mut p = vec![prev.markdown_path];
                if let Some(raw) = prev.raw_path {
                    p.push(raw);
                }
                p
            }
            None => vec![],
        }
    } else {
        vec![]
    };
    drop(rtx);

    // 4. Build the records.
    let source = input
        .source
        .clone()
        .unwrap_or(KbSource::Doc { path: PathBuf::from("(manual)") });
    let visibility = input
        .visibility
        .clone()
        .unwrap_or_else(|| KbVisibility::default_for(input.canon.metadata.source_kind));
    let doc = KbDoc {
        id: doc_id.clone(),
        logical_source_id: lsid_str.clone(),
        source,
        source_kind: input.canon.metadata.source_kind,
        title: input.canon.metadata.title.clone(),
        mime: input.canon.metadata.mime.clone(),
        raw_sha256: raw_sha.clone(),
        markdown_path: staged.markdown_rel_path.clone(),
        markdown_sha256: staged.markdown_sha256.clone(),
        raw_path: staged.raw_rel_path.clone(),
        owner_user_id: input.owner_user_id.clone(),
        created_at: now_ms,
        updated_at: now_ms,
        version: next_version,
        status: KbStatus::Active,
        visibility,
        tags: input.canon.metadata.tags.clone(),
        meta: input.canon.metadata.extra.clone(),
    };

    let mut ledger_new_paths = vec![staged.markdown_rel_path.clone()];
    if let Some(raw_rel) = &staged.raw_rel_path {
        ledger_new_paths.push(raw_rel.clone());
    }
    let ledger_entry = IngestLedgerEntry {
        id: ulid::Ulid::new().to_string(),
        created_at: now_ms,
        updated_at: now_ms,
        doc_id: doc_id.clone(),
        logical_source_id: lsid_str.clone(),
        op: if next_version == 1 { LedgerOp::Create } else { LedgerOp::Update },
        new_paths: ledger_new_paths,
        old_paths,
        status: LedgerStatus::Pending,
        error: None,
    };

    let job = Job::new(JobKind::ChunkAndEmbed {
        doc_id: doc_id.clone(),
        doc_version: next_version,
    });

    // 5. Single write tx: persist all five records together.
    {
        let wtx = store.begin_write()?;
        docs::put(&wtx, &doc)?;
        docs::set_latest_version(
            &wtx,
            &lsid_str,
            &VersionPointer { doc_id: doc_id.clone(), version: next_version },
        )?;
        ledger::put(&wtx, &ledger_entry)?;
        jobs::enqueue(&wtx, &job)?;
        if let Some((source_id, item_id)) = input.seen_key {
            seen::mark_seen(&wtx, source_id, item_id, &raw_sha, now_ms)?;
        }
        wtx.commit()?;
    }

    tracing::info!(
        doc = %crate::kb::redact(&doc_id),
        version = next_version,
        "kb ingest: committed"
    );

    Ok(IngestOutput {
        doc_id,
        noop: false,
        markdown_rel_path: staged.markdown_rel_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::content_store::atomic::sha256_hex;
    use crate::kb::jobs::JobStatus;
    use crate::kb::ledger::LedgerStatus;
    use crate::kb::store::{jobs as jobs_store, ledger as ledger_store};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, KbStore, KbPaths) {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let paths = KbPaths::new(tmp.path().join("kb"));
        paths.ensure_layout().unwrap();
        (tmp, store, paths)
    }

    fn canon(body: &str) -> CanonicalizedSource {
        let bytes = body.as_bytes();
        canonicalize_by_mime(CanonicalizeInput {
            bytes,
            mime: "text/markdown",
            hint_title: Some("title"),
            logical_source_id_seed: None,
        })
        .unwrap()
        .unwrap()
    }

    #[test]
    fn fresh_ingest_writes_all_tables() {
        let (_tmp, store, paths) = fixture();
        let c = canon("# Hello\n\nbody.");
        let raw = b"# Hello\n\nbody.";
        let out = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c,
                raw_bytes: raw,
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: Some(("manual", "f1")),
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        assert!(!out.noop);
        assert!(out.markdown_rel_path.starts_with("md/doc/"));

        let rtx = store.begin_read().unwrap();
        let doc = docs::get(&rtx, &out.doc_id).unwrap().unwrap();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.raw_sha256, sha256_hex(raw));

        let ptr = docs::latest_version(&rtx, &c.metadata.logical_source_id.0).unwrap().unwrap();
        assert_eq!(ptr.doc_id, out.doc_id);

        let pending = ledger_store::list_by_status(&rtx, LedgerStatus::Pending).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].doc_id, out.doc_id);

        let ready_jobs = jobs_store::list_by_status(&rtx, JobStatus::Ready).unwrap();
        assert_eq!(ready_jobs.len(), 1);
        assert!(matches!(ready_jobs[0].kind, JobKind::ChunkAndEmbed { .. }));

        let seen = seen::is_seen(&rtx, "manual", "f1").unwrap().unwrap();
        assert_eq!(seen.raw_sha256, sha256_hex(raw));
    }

    #[test]
    fn reingest_same_bytes_noops() {
        let (_tmp, store, paths) = fixture();
        let c = canon("identical body");
        let raw = b"identical body";
        let first = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c, raw_bytes: raw, raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        )
        .unwrap();
        let second = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c, raw_bytes: raw, raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        )
        .unwrap();
        assert_eq!(first.doc_id, second.doc_id);
        assert!(second.noop);
        // No second job should have been enqueued.
        let rtx = store.begin_read().unwrap();
        assert_eq!(jobs_store::list_by_status(&rtx, JobStatus::Ready).unwrap().len(), 1);
    }

    #[test]
    fn reingest_different_bytes_bumps_version() {
        let (_tmp, store, paths) = fixture();
        // Same logical source, different bytes ⇒ same lsid only if
        // logical_source_id_seed is reused. The default canonicalizer
        // derives lsid from sha256(bytes), so we must pass an explicit
        // seed to simulate "same logical source, new content".
        use crate::kb::canonicalize::CanonicalizeInput;
        use crate::kb::model::LogicalSourceId;
        let lsid = LogicalSourceId("file:custom:x".into());
        let c1 = crate::kb::canonicalize::canonicalize_by_mime(CanonicalizeInput {
            bytes: b"version 1",
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: Some(lsid.clone()),
        })
        .unwrap()
        .unwrap();
        let c2 = crate::kb::canonicalize::canonicalize_by_mime(CanonicalizeInput {
            bytes: b"version 2 different",
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: Some(lsid.clone()),
        })
        .unwrap()
        .unwrap();

        let a = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c1, raw_bytes: b"version 1", raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        )
        .unwrap();
        let b = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c2, raw_bytes: b"version 2 different", raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        )
        .unwrap();
        assert_ne!(a.doc_id, b.doc_id);
        assert!(!b.noop);

        let rtx = store.begin_read().unwrap();
        let doc_a = docs::get(&rtx, &a.doc_id).unwrap().unwrap();
        let doc_b = docs::get(&rtx, &b.doc_id).unwrap().unwrap();
        assert_eq!(doc_a.version, 1);
        assert_eq!(doc_b.version, 2);

        // Ledger b should have old_paths = [doc_a.markdown_path, doc_a.raw_path]
        let ledgers = ledger_store::list_by_status(&rtx, LedgerStatus::Pending).unwrap();
        let lb = ledgers.iter().find(|e| e.doc_id == b.doc_id).unwrap();
        assert!(lb.old_paths.contains(&doc_a.markdown_path));
    }
}
```

Update `src/kb/pipeline/mod.rs`:

```rust
pub mod ingest;
pub use ingest::{ingest_canonicalized, IngestInput, IngestOutput};
```

And re-export from `src/kb/mod.rs`:

```rust
pub use pipeline::{ingest_canonicalized, IngestInput, IngestOutput};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::pipeline::ingest
git add src/kb/pipeline/ src/kb/mod.rs
git commit -m "feat(kb): ingest_canonicalized — atomic single-tx pipeline (doc + ledger + job + seen + version)"
```

---

## Task 11: `worker/handlers/` — JobHandler trait + ChunkAndEmbed handler

**Files:** `src/kb/worker/handlers/mod.rs`, `src/kb/worker/handlers/chunk_embed.rs`

The handler reads the staged markdown, runs the chunker, embeds chunks via the configured `KbEmbedder`, writes them into `kb_chunks` (single tx), and updates the ledger to `IndexingComplete`. Must be idempotent: re-running on the same doc must produce the same chunk_ids and overwrite the same redb rows.

- [ ] **Step 1: Write trait + dispatch + ChunkAndEmbed**

```rust
// src/kb/worker/handlers/mod.rs

pub mod chunk_embed;

use crate::kb::embedder::KbEmbedder;
use crate::kb::jobs::JobKind;
use crate::kb::paths::KbPaths;
use crate::kb::store::KbStore;
use anyhow::Result;
use std::sync::Arc;

/// Job handler. One impl per `JobKind` variant. Handlers must be
/// **idempotent** — the worker reclaim path will re-run them after
/// stalled claims.
pub trait JobHandler: Send + Sync {
    fn handle(&self, ctx: &HandlerCtx, kind: &JobKind) -> Result<()>;
}

pub struct HandlerCtx {
    pub store: Arc<KbStore>,
    pub paths: Arc<KbPaths>,
    pub embedder: Arc<dyn KbEmbedder>,
}

/// Default dispatcher: matches on `JobKind` and delegates.
pub struct DefaultDispatcher;

impl JobHandler for DefaultDispatcher {
    fn handle(&self, ctx: &HandlerCtx, kind: &JobKind) -> Result<()> {
        match kind {
            JobKind::ChunkAndEmbed { doc_id, doc_version } => {
                chunk_embed::run(ctx, doc_id, *doc_version)
            }
            JobKind::RebuildHnsw => {
                tracing::warn!("kb worker: RebuildHnsw handler not implemented in Week 2");
                Ok(())
            }
            JobKind::RunCompactor => {
                tracing::warn!("kb worker: RunCompactor handler not implemented in Week 2");
                Ok(())
            }
        }
    }
}
```

```rust
// src/kb/worker/handlers/chunk_embed.rs

//! ChunkAndEmbed handler: read staged markdown, chunk, embed, write
//! chunks to redb, mark ledger IndexingComplete. Idempotent —
//! deterministic chunk_ids mean re-running produces identical rows.

use crate::kb::chunker::{chunk_markdown, ChunkerInput, LocatorKind};
use crate::kb::content_store::read::read_doc_body;
use crate::kb::ledger::LedgerStatus;
use crate::kb::model::{KbChunk, LogicalSourceId};
use crate::kb::store::{chunks, docs, ledger};
use crate::kb::worker::handlers::HandlerCtx;
use anyhow::{Context, Result};

pub fn run(ctx: &HandlerCtx, doc_id: &str, doc_version: u32) -> Result<()> {
    // 1. Load doc + body.
    let doc = {
        let rtx = ctx.store.begin_read()?;
        docs::get(&rtx, doc_id)?
            .ok_or_else(|| anyhow::anyhow!("chunk_embed: doc {doc_id} not found"))?
    };
    if doc.version != doc_version {
        tracing::warn!(
            doc = %crate::kb::redact(doc_id),
            "kb worker: doc version mismatch (job v{doc_version} vs current v{}); skipping",
            doc.version
        );
        return Ok(());
    }
    let abs = ctx.paths.root.join(&doc.markdown_path);
    let body = read_doc_body(&abs).with_context(|| format!("read body {}", abs.display()))?;

    // 2. Chunk.
    let lsid = LogicalSourceId(doc.logical_source_id.clone());
    let chunks_vec: Vec<KbChunk> = chunk_markdown(ChunkerInput {
        logical_source_id: &lsid,
        doc_id: &doc.id,
        doc_version: doc.version,
        markdown_body: &body,
        default_locator_kind: LocatorKind::MdSection,
    });

    // 3. Embed.
    let texts: Vec<String> = chunks_vec.iter().map(|c| c.indexed_text.clone()).collect();
    let vectors = ctx.embedder.embed_batch(&texts)?;
    if vectors.len() != chunks_vec.len() {
        return Err(anyhow::anyhow!(
            "embedder returned {} vectors for {} chunks",
            vectors.len(),
            chunks_vec.len()
        ));
    }
    let embedder_id = ctx.embedder.embedder_id().to_string();
    let chunks_with_vec: Vec<KbChunk> = chunks_vec
        .into_iter()
        .zip(vectors)
        .map(|(mut c, v)| {
            c.vector = v;
            c.embedder_id = embedder_id.clone();
            c
        })
        .collect();

    // 4. Persist chunks + advance ledger in one tx.
    {
        let wtx = ctx.store.begin_write()?;
        for c in &chunks_with_vec {
            chunks::put(&wtx, c)?;
        }
        // Find the ledger entry for this doc (latest Pending for this doc_id).
        if let Some(ledger_id) = find_ledger_for_doc(&ctx.store, &doc.id)? {
            let now_ms = chrono::Utc::now().timestamp_millis();
            ledger::update_status(&wtx, &ledger_id, LedgerStatus::IndexingComplete, now_ms)?;
        }
        wtx.commit()?;
    }

    tracing::info!(
        doc = %crate::kb::redact(doc_id),
        n_chunks = chunks_with_vec.len(),
        "kb worker: chunk_embed complete"
    );
    Ok(())
}

fn find_ledger_for_doc(store: &crate::kb::store::KbStore, doc_id: &str) -> Result<Option<String>> {
    let rtx = store.begin_read()?;
    for entry in ledger::list_by_status(&rtx, LedgerStatus::Pending)? {
        if entry.doc_id == doc_id {
            return Ok(Some(entry.id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::embedder::StubEmbedder;
    use crate::kb::paths::KbPaths;
    use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
    use crate::kb::store::KbStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, HandlerCtx, String) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

        let bytes = b"# Hi\n\nbody one.\n\nbody two.";
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
                canon: &canon, raw_bytes: bytes, raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        )
        .unwrap();
        let ctx = HandlerCtx { store, paths, embedder };
        (tmp, ctx, out.doc_id)
    }

    #[test]
    fn writes_chunks_with_vectors() {
        let (_tmp, ctx, doc_id) = fixture();
        let doc = {
            let rtx = ctx.store.begin_read().unwrap();
            docs::get(&rtx, &doc_id).unwrap().unwrap()
        };
        run(&ctx, &doc_id, doc.version).unwrap();
        let rtx = ctx.store.begin_read().unwrap();
        let cs = chunks::chunks_for_logical(&rtx, &doc.logical_source_id).unwrap();
        assert!(!cs.is_empty());
        for c in &cs {
            assert_eq!(c.vector.len(), 1024);
            assert_eq!(c.embedder_id, "stub-sha256-1024");
        }
    }

    #[test]
    fn idempotent_rerun_produces_same_chunks() {
        let (_tmp, ctx, doc_id) = fixture();
        let doc = {
            let rtx = ctx.store.begin_read().unwrap();
            docs::get(&rtx, &doc_id).unwrap().unwrap()
        };
        run(&ctx, &doc_id, doc.version).unwrap();
        let before = {
            let rtx = ctx.store.begin_read().unwrap();
            chunks::chunks_for_logical(&rtx, &doc.logical_source_id).unwrap()
        };
        run(&ctx, &doc_id, doc.version).unwrap();
        let after = {
            let rtx = ctx.store.begin_read().unwrap();
            chunks::chunks_for_logical(&rtx, &doc.logical_source_id).unwrap()
        };
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(after.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.vector, b.vector);
        }
    }

    #[test]
    fn ledger_advances_to_indexing_complete() {
        let (_tmp, ctx, doc_id) = fixture();
        let doc = {
            let rtx = ctx.store.begin_read().unwrap();
            docs::get(&rtx, &doc_id).unwrap().unwrap()
        };
        run(&ctx, &doc_id, doc.version).unwrap();
        let rtx = ctx.store.begin_read().unwrap();
        let done = ledger::list_by_status(&rtx, LedgerStatus::IndexingComplete).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].doc_id, doc_id);
    }
}
```

Update `src/kb/worker/mod.rs`:

```rust
pub mod handlers;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::worker::handlers
git add src/kb/worker/
git commit -m "feat(kb): JobHandler trait + ChunkAndEmbed handler (idempotent)"
```

---

## Task 12: `worker/pool.rs` — tokio worker pool

**Files:** `src/kb/worker/pool.rs`, modify `src/kb/worker/mod.rs`

A single tokio task that loops: claim → run handler → mark done/failed → sleep briefly if no jobs. A second background task periodically calls `reclaim_stale`. Both tasks share the same `KbStore`.

- [ ] **Step 1: Write impl + tests**

```rust
//! Worker pool: a single tokio task that claims + runs jobs, with
//! `reclaim_stale` interleaved every `reclaim_interval`. Single-task
//! design keeps shutdown observation trivial — the loop checks an
//! `AtomicBool` at the top of each iteration and on every wake from
//! the idle sleep.
//!
//! Multi-worker support is a Week 3 follow-up (spawn N copies of
//! `run_main` that share the same `KbStore`; redb's single-writer
//! semantics serialise the claim contention naturally).

use crate::kb::store::{jobs, KbStore};
use crate::kb::worker::handlers::{DefaultDispatcher, HandlerCtx, JobHandler};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub worker_id: String,
    pub claim_ttl_ms: i64,
    pub poll_idle: Duration,
    pub reclaim_interval: Duration,
    pub max_attempts: u32,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: format!("worker-{}", ulid::Ulid::new()),
            claim_ttl_ms: 60_000,
            poll_idle: Duration::from_millis(100),
            reclaim_interval: Duration::from_secs(30),
            max_attempts: 5,
        }
    }
}

pub struct WorkerPool {
    main: JoinHandle<()>,
    shutdown: Arc<AtomicBool>,
}

impl WorkerPool {
    /// Start with `DefaultDispatcher`.
    pub fn start(ctx: HandlerCtx, cfg: WorkerConfig) -> Self {
        Self::start_with_handler(ctx, cfg, Arc::new(DefaultDispatcher))
    }

    /// Start with a custom job handler. Tests use this with handlers
    /// that fail deterministically to exercise the retry path.
    pub fn start_with_handler(
        ctx: HandlerCtx,
        cfg: WorkerConfig,
        handler: Arc<dyn JobHandler>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let main = tokio::spawn(run_main(ctx, cfg, handler, shutdown.clone()));
        Self { main, shutdown }
    }

    /// Signal shutdown and wait for the background task to drain its
    /// current job (if any) and exit.
    pub async fn shutdown(self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.main.await;
    }

    /// Test helper: run exactly one job synchronously. Returns true if
    /// a job was claimed and processed. Used in tests so we don't have
    /// to poll-and-wait.
    pub fn run_one_blocking(
        ctx: &HandlerCtx,
        cfg: &WorkerConfig,
        handler: &dyn JobHandler,
    ) -> Result<bool> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let claimed = {
            let wtx = ctx.store.begin_write()?;
            let claim = jobs::claim_next(&wtx, &cfg.worker_id, now_ms, cfg.claim_ttl_ms)?;
            wtx.commit()?;
            claim
        };
        let Some((job, _token)) = claimed else {
            return Ok(false);
        };
        match handler.handle(ctx, &job.kind) {
            Ok(()) => {
                let wtx = ctx.store.begin_write()?;
                jobs::mark_done(&wtx, &job.id)?;
                wtx.commit()?;
                Ok(true)
            }
            Err(e) => {
                let wtx = ctx.store.begin_write()?;
                if job.attempts + 1 >= cfg.max_attempts {
                    jobs::mark_failed(&wtx, &job.id, &format!("{e:#}"))?;
                } else {
                    jobs::requeue(&wtx, &job.id)?;
                }
                wtx.commit()?;
                Ok(true)
            }
        }
    }
}

async fn run_main(
    ctx: HandlerCtx,
    cfg: WorkerConfig,
    handler: Arc<dyn JobHandler>,
    shutdown: Arc<AtomicBool>,
) {
    let mut next_reclaim = Instant::now() + cfg.reclaim_interval;
    while !shutdown.load(Ordering::Acquire) {
        // Periodic reclaim sweep — single task interleaves it with
        // claim attempts so we don't need a second task + its own
        // shutdown wiring.
        if Instant::now() >= next_reclaim {
            run_reclaim_once(&ctx.store);
            next_reclaim = Instant::now() + cfg.reclaim_interval;
        }

        let did_work = match WorkerPool::run_one_blocking(&ctx, &cfg, handler.as_ref()) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("kb worker main loop error: {e:#}");
                false
            }
        };
        if !did_work {
            // Shutdown-aware sleep so the test's `pool.shutdown().await`
            // doesn't have to wait the full `poll_idle` to return.
            let deadline = Instant::now() + cfg.poll_idle;
            while Instant::now() < deadline {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

fn run_reclaim_once(store: &KbStore) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let res = (|| -> Result<usize> {
        let wtx = store.begin_write()?;
        let n = jobs::reclaim_stale(&wtx, now_ms)?.len();
        wtx.commit()?;
        Ok(n)
    })();
    match res {
        Ok(n) if n > 0 => tracing::info!("kb worker: reclaimed {n} stale jobs"),
        Ok(_) => {}
        Err(e) => tracing::error!("kb worker reclaim error: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::embedder::{KbEmbedder, StubEmbedder};
    use crate::kb::jobs::JobStatus;
    use crate::kb::paths::KbPaths;
    use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
    use crate::kb::store::{chunks as chunks_store, jobs as jobs_store};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, HandlerCtx, WorkerConfig, String, String) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

        let bytes = b"# T\n\nbody.";
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes,
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: None,
        })
        .unwrap()
        .unwrap();
        let lsid = canon.metadata.logical_source_id.0.clone();
        let out = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon, raw_bytes: bytes, raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        )
        .unwrap();

        let ctx = HandlerCtx { store, paths, embedder };
        let cfg = WorkerConfig { worker_id: "w-test".into(), ..WorkerConfig::default() };
        (tmp, ctx, cfg, out.doc_id, lsid)
    }

    #[test]
    fn run_one_processes_ready_job() {
        let (_tmp, ctx, cfg, _doc_id, lsid) = fixture();
        let handler = DefaultDispatcher;
        assert!(WorkerPool::run_one_blocking(&ctx, &cfg, &handler).unwrap());
        // After running: chunks exist, job is Done.
        let rtx = ctx.store.begin_read().unwrap();
        assert!(!chunks_store::chunks_for_logical(&rtx, &lsid).unwrap().is_empty());
        assert!(jobs_store::list_by_status(&rtx, JobStatus::Ready).unwrap().is_empty());
        let done = jobs_store::list_by_status(&rtx, JobStatus::Done).unwrap();
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn run_one_returns_false_when_idle() {
        let (_tmp, ctx, cfg, _doc_id, _lsid) = fixture();
        let handler = DefaultDispatcher;
        // Drain the one queued job, then assert idle.
        assert!(WorkerPool::run_one_blocking(&ctx, &cfg, &handler).unwrap());
        assert!(!WorkerPool::run_one_blocking(&ctx, &cfg, &handler).unwrap());
    }

    #[test]
    fn handler_error_requeues_until_max_attempts() {
        let (_tmp, ctx, mut cfg, _doc_id, _lsid) = fixture();
        cfg.max_attempts = 2;
        struct AlwaysFails;
        impl JobHandler for AlwaysFails {
            fn handle(&self, _: &HandlerCtx, _: &crate::kb::jobs::JobKind) -> Result<()> {
                Err(anyhow::anyhow!("nope"))
            }
        }
        let h = AlwaysFails;
        // Attempt 1 → requeue (attempts becomes 1)
        WorkerPool::run_one_blocking(&ctx, &cfg, &h).unwrap();
        let rtx = ctx.store.begin_read().unwrap();
        let ready = jobs_store::list_by_status(&rtx, JobStatus::Ready).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].attempts, 1);
        drop(rtx);
        // Attempt 2 → max_attempts reached → mark_failed
        WorkerPool::run_one_blocking(&ctx, &cfg, &h).unwrap();
        let rtx = ctx.store.begin_read().unwrap();
        let failed = jobs_store::list_by_status(&rtx, JobStatus::Failed).unwrap();
        assert_eq!(failed.len(), 1);
    }
}
```

Update `src/kb/worker/mod.rs`:

```rust
pub mod handlers;
pub mod pool;
pub use handlers::{DefaultDispatcher, HandlerCtx, JobHandler};
pub use pool::{WorkerConfig, WorkerPool};
```

Re-export from `src/kb/mod.rs`:

```rust
pub use worker::{DefaultDispatcher, HandlerCtx, JobHandler, WorkerConfig, WorkerPool};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::worker::pool
git add src/kb/worker/ src/kb/mod.rs
git commit -m "feat(kb): WorkerPool (tokio task + reclaim + retry/max_attempts)"
```

---

## Task 13: Crash-recovery integration test — reclaim_stale path

**Files:** `tests/kb_week2_recovery.rs`

The redb transaction semantics already make "mid-tx crash" trivially safe (uncommitted txs are atomic — they all-or-nothing). The interesting recovery scenarios are:
1. Worker dies after claiming, before marking done → claim expires → reclaim sets back to Ready → another worker re-runs → idempotent.
2. Process restart after `ingest_canonicalized` commit but before worker picks the job → restart, worker queue resumes.

This test exercises both paths end-to-end.

- [ ] **Step 1: Write integration test**

```rust
//! Crash recovery integration tests for the KB ingest + worker
//! pipeline. See spec §J 崩溃恢复矩阵.

use anyhow::Result;
use rsclaw::kb::{
    canonicalize_by_mime, ingest_canonicalized,
    store::{chunks, jobs as jobs_store},
    CanonicalizeInput, DefaultDispatcher, HandlerCtx, IngestInput, KbEmbedder, KbPaths,
    KbStore, StubEmbedder, WorkerConfig, WorkerPool,
};
use std::sync::Arc;
use tempfile::TempDir;

fn pipeline_fixture() -> (TempDir, HandlerCtx, WorkerConfig, String, String) {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout().unwrap();
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

    let bytes = b"# Recovery test\n\nbody one.\n\nbody two.";
    let canon = canonicalize_by_mime(CanonicalizeInput {
        bytes,
        mime: "text/markdown",
        hint_title: Some("recovery"),
        logical_source_id_seed: None,
    })
    .unwrap()
    .unwrap();
    let lsid = canon.metadata.logical_source_id.0.clone();
    let out = ingest_canonicalized(
        &store,
        IngestInput {
            canon: &canon, raw_bytes: bytes, raw_ext: "md",
            visibility: None, owner_user_id: None, seen_key: None,
            source: None, paths: &paths,
        },
    )
    .unwrap();
    let ctx = HandlerCtx { store, paths, embedder };
    let cfg = WorkerConfig {
        worker_id: "w-recovery".into(),
        claim_ttl_ms: 50,            // short TTL for fast reclaim
        ..WorkerConfig::default()
    };
    (tmp, ctx, cfg, out.doc_id, lsid)
}

/// Scenario: worker claims a job, dies mid-handler (we simulate by
/// holding the claim without marking Done), TTL expires, reclaim
/// resets the job, another worker re-runs the handler → chunks land
/// exactly once (idempotency via deterministic chunk_id).
#[test]
fn stalled_claim_is_reclaimed_and_rerun() -> Result<()> {
    let (_tmp, ctx, cfg, _doc_id, lsid) = pipeline_fixture();

    // Step 1: First worker claims but does NOT mark done.
    {
        let wtx = ctx.store.begin_write()?;
        let _ = jobs_store::claim_next(&wtx, "w-zombie", 100, cfg.claim_ttl_ms)?;
        wtx.commit()?;
    }
    // Step 2: Wait past TTL.
    std::thread::sleep(std::time::Duration::from_millis(100));
    // Step 3: Reclaim — claim was made at t=100 with TTL=50, so
    // expires_at=150; now_ms=300 → stale.
    let reclaimed = {
        let wtx = ctx.store.begin_write()?;
        let r = jobs_store::reclaim_stale(&wtx, 300)?;
        wtx.commit()?;
        r
    };
    assert_eq!(reclaimed.len(), 1);
    // Step 4: New worker picks up + runs.
    let handler = DefaultDispatcher;
    assert!(WorkerPool::run_one_blocking(&ctx, &cfg, &handler)?);
    // Step 5: Chunks landed exactly once.
    let rtx = ctx.store.begin_read()?;
    let cs = chunks::chunks_for_logical(&rtx, &lsid)?;
    assert!(!cs.is_empty());
    // No duplicates (chunk_ids are unique).
    let mut ids = cs.iter().map(|c| c.id.clone()).collect::<Vec<_>>();
    ids.sort();
    let len_before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), len_before, "duplicate chunk_ids");
    Ok(())
}

/// Scenario: ingest commits → process restart (simulated by dropping
/// the KbStore handle and reopening) → worker drains the queued job.
/// Confirms the Outbox pattern survives restart.
#[test]
fn ingest_survives_process_restart() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("kb.redb");
    let kb_root = tmp.path().join("kb");
    let canon_bytes = b"# Restart\n\nbody.";
    let lsid = {
        let store = Arc::new(KbStore::open(&db_path)?);
        let paths = Arc::new(KbPaths::new(&kb_root));
        paths.ensure_layout()?;
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: canon_bytes,
            mime: "text/markdown",
            hint_title: Some("r"),
            logical_source_id_seed: None,
        })?
        .unwrap();
        let lsid = canon.metadata.logical_source_id.0.clone();
        ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon, raw_bytes: canon_bytes, raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        )?;
        lsid
        // store + paths dropped here → simulates process exit
    };

    // "Restart" — reopen the store + worker.
    let store = Arc::new(KbStore::open(&db_path)?);
    let paths = Arc::new(KbPaths::new(&kb_root));
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let ctx = HandlerCtx { store: store.clone(), paths, embedder };
    let cfg = WorkerConfig::default();
    let handler = DefaultDispatcher;
    assert!(WorkerPool::run_one_blocking(&ctx, &cfg, &handler)?);

    let rtx = store.begin_read()?;
    assert!(!chunks::chunks_for_logical(&rtx, &lsid)?.is_empty());
    Ok(())
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test --test kb_week2_recovery
git add tests/kb_week2_recovery.rs
git commit -m "test(kb): crash recovery — reclaim_stale + process restart"
```

---

## Task 14: e2e integration test — full pipeline through async worker

**Files:** `tests/kb_week2_pipeline.rs`

This test exercises the actual async path: spawn `WorkerPool::start`, ingest a doc, poll until chunks appear in redb, then `shutdown` and assert.

- [ ] **Step 1: Write integration test**

```rust
//! Week 2 end-to-end: ingest_canonicalized → WorkerPool drains job
//! asynchronously → chunks land in redb. Verifies the production
//! async path (not just `run_one_blocking`).

use anyhow::Result;
use rsclaw::kb::{
    canonicalize_by_mime, ingest_canonicalized,
    store::chunks,
    CanonicalizeInput, HandlerCtx, IngestInput, KbEmbedder, KbPaths, KbStore, StubEmbedder,
    WorkerConfig, WorkerPool,
};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
async fn end_to_end_ingest_then_worker_drains_async() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

    let ctx = HandlerCtx {
        store: store.clone(),
        paths: paths.clone(),
        embedder,
    };
    let cfg = WorkerConfig {
        worker_id: "w-e2e".into(),
        poll_idle: Duration::from_millis(20),
        ..WorkerConfig::default()
    };
    let pool = WorkerPool::start(ctx, cfg);

    // Ingest a doc.
    let bytes = b"# E2E\n\nfirst.\n\nsecond.\n\nthird.";
    let canon = canonicalize_by_mime(CanonicalizeInput {
        bytes,
        mime: "text/markdown",
        hint_title: Some("e2e"),
        logical_source_id_seed: None,
    })?
    .unwrap();
    let lsid = canon.metadata.logical_source_id.0.clone();
    ingest_canonicalized(
        &store,
        IngestInput {
            canon: &canon, raw_bytes: bytes, raw_ext: "md",
            visibility: None, owner_user_id: None, seen_key: None,
            source: None, paths: &paths,
        },
    )?;

    // Wait up to 2s for the worker to chunk + embed.
    let wait = timeout(Duration::from_secs(2), async {
        loop {
            let rtx = store.begin_read().unwrap();
            let cs = chunks::chunks_for_logical(&rtx, &lsid).unwrap();
            if !cs.is_empty() {
                return cs;
            }
            drop(rtx);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert!(!wait.is_empty(), "worker never produced chunks");
    for c in &wait {
        assert_eq!(c.vector.len(), 1024);
    }
    pool.shutdown().await;
    Ok(())
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test --test kb_week2_pipeline
git add tests/kb_week2_pipeline.rs
git commit -m "test(kb): Week 2 e2e — async ingest → WorkerPool → chunks in redb"
```

---

## Task 15: Wire kb tests + integration tests into CI smoke

**Files:** (none — verification only)

- [ ] **Step 1: Run the full kb test surface**

```bash
cargo test -p rsclaw --lib kb::            # all unit tests
cargo test --test kb_week1_e2e             # Week 1 e2e (regression check)
cargo test --test kb_week2_pipeline        # Week 2 async e2e
cargo test --test kb_week2_recovery        # crash recovery
```

Expected: all pass; new tests = ~50 unit + 3 integration. Total kb unit count crosses ~160.

- [ ] **Step 2: (No commit if green; document if any flake found)**

If a test is flaky (especially the async e2e timeout), open a follow-up issue rather than landing a sleep-and-pray retry.

---

## Task 16: Update `src/kb/README.md` with Week 2 scope

**Files:** `src/kb/README.md`

- [ ] **Step 1: Replace the "What's implemented (Week 1)" section to include Week 2**

Update the two top-level sections to:

````markdown
## What's implemented (Weeks 1–2)

**Week 1 (Foundation):**

- Types, content store, canonicalizers, chunker, redb schema, file IO primitives.

**Week 2 (Persistence + Pipeline):**

- **redb accessors** (`store/docs`, `store/chunks`, `store/seen`,
  `store/ledger`, `store/jobs`) — composable inside a single
  `WriteTransaction` so the pipeline can write doc + ledger + job +
  seen atomically.
- **`KbStore` facade** — owns the `redb::Database`, exposes
  `begin_write`/`begin_read`.
- **`KbEmbedder` trait + `StubEmbedder`** — deterministic 1024-dim
  vectors for tests; real BGE-M3 embedder lands as a self-contained
  follow-up behind the same trait.
- **`ingest_canonicalized()`** — single-tx atomic pipeline (NOOP
  short-circuit + stage + 5-table write + commit). Returns `doc_id`
  synchronously.
- **`WorkerPool`** — tokio task that claims `Ready` jobs from
  `kb_jobs_by_status_priority`, dispatches to `JobHandler`, marks
  `Done` / `Failed` / requeues. `reclaim_stale` runs periodically
  for expired claims.
- **`ChunkAndEmbed` handler** — reads staged markdown, runs the
  Week 1 chunker, embeds via `KbEmbedder`, writes chunks + advances
  ledger to `IndexingComplete`. Idempotent.
- **Crash recovery** — stalled-claim reclaim path tested; process
  restart resumes the queue.

## What's NOT in Weeks 1–2

- BGE-M3 embedder (real model) — Week 2.5 (self-contained behind `KbEmbedder` trait)
- Tantivy `add_document` — Week 3
- HNSW `insert` + ArcSwap cache — Week 3
- Hybrid retrieval (RRF + MMR) / `kb_search` tool — Week 3
- Visibility filter wiring into retrieval — Week 3
- URL fetch + HTML→Markdown (`UrlCanonicalizer`) — Week 4 (with `UrlSyncer`)
- `ManualUploadSyncer` + `UrlSyncer` + CLI — Week 4
- Compactor (orphan file cleanup + ledger advancement past `IndexingComplete`) — Week 4
````

- [ ] **Step 2: Add Week 2 architecture invariants**

Append after the existing Week 1 invariants:

```markdown
### Added in Week 2

8. **All ingest writes happen in one redb tx** — `ingest_canonicalized`
   commits `KbDoc` + `VersionPointer` + `IngestLedgerEntry` + `Job` +
   `SeenItems` together. Splitting any of these into separate txs
   reintroduces the Outbox bug: a doc visible to readers but no job
   queued for chunking. Covered by
   `kb::pipeline::ingest::tests::fresh_ingest_writes_all_tables`.
9. **`ChunkAndEmbed` handler is idempotent** — re-running on the same
   `doc_id` produces identical chunks (deterministic `chunk_id`) and
   identical vectors (deterministic `StubEmbedder` / future real
   embedder is also deterministic per text input). Covered by
   `kb::worker::handlers::chunk_embed::tests::idempotent_rerun_produces_same_chunks`.
10. **Job dedupe is keyed on `JobKind::dedupe_key()`, not job_id** —
    enqueueing the same logical work twice while a job is `Ready` or
    `Running` returns the existing `job_id` without writing a duplicate.
    Covered by `kb::store::jobs::tests::enqueue_dedupes_active_jobs`.
11. **Stalled claims auto-reclaim** — workers that crash mid-job leave
    a claim with `expires_at` in the past; the next `reclaim_stale`
    sweep resets the job to `Ready` and another worker re-runs it.
    Covered by `tests/kb_week2_recovery.rs::stalled_claim_is_reclaimed_and_rerun`.
```

- [ ] **Step 3: Bump the "Quick start" example**

Replace the Week 1 Quick start with one that uses `ingest_canonicalized` + `WorkerPool`:

````markdown
## Quick start (Weeks 1–2)

```rust
use rsclaw::kb::{
    canonicalize_by_mime, detect_mime, ingest_canonicalized,
    CanonicalizeInput, HandlerCtx, IngestInput, KbEmbedder, KbPaths, KbStore,
    StubEmbedder, WorkerConfig, WorkerPool,
};
use std::sync::Arc;

# async fn demo() -> anyhow::Result<()> {
let store = Arc::new(KbStore::open(std::path::Path::new(".rsclaw/kb/db/kb.redb"))?);
let paths = Arc::new(KbPaths::new(".rsclaw/kb"));
paths.ensure_layout()?;
let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

// Start the worker pool in the background.
let pool = WorkerPool::start(
    HandlerCtx { store: store.clone(), paths: paths.clone(), embedder },
    WorkerConfig::default(),
);

// Synchronously ingest a doc; worker picks up the job asynchronously.
let bytes = std::fs::read("manual.md")?;
let mime = detect_mime(&bytes, Some("manual.md"));
let canon = canonicalize_by_mime(CanonicalizeInput {
    bytes: &bytes, mime: &mime,
    hint_title: Some("manual.md"), logical_source_id_seed: None,
})?.unwrap();
let out = ingest_canonicalized(
    &store,
    IngestInput {
        canon: &canon, raw_bytes: &bytes, raw_ext: "md",
        visibility: None, owner_user_id: None, seen_key: None,
        source: None, paths: &paths,
    },
)?;
println!("ingested doc_id = {} (noop={})", out.doc_id, out.noop);

// ... worker chunks + embeds in the background. Tantivy + HNSW
// indexing + retrieval land in Week 3.

pool.shutdown().await;
# Ok(()) }
```
````

- [ ] **Step 4: Commit**

```bash
git add src/kb/README.md
git commit -m "docs(kb): Week 2 README — pipeline + worker pool + invariants 8-11"
```

---

## Open questions (resolve as you implement)

These mirror the ones still listed in the spec / ADR Open Questions section. Address each one during the task it touches and either resolve in code or add a `TODO(kb-week3)` with a one-line rationale:

- **Worker pool size** — Week 2 ships a single-worker pool. Multi-worker support is trivial (start N tasks that share the same `KbStore`) but adds the question of how aggressively to interleave claims vs the natural single-writer redb serialisation. Decide based on profiling Week 3's retrieval load.
- **Retry/backoff policy** — Week 2 uses linear retry up to `max_attempts`. Exponential backoff (re-enqueue with a future `created_at`) is a Week 3 follow-up if Week 2 logs show hot retry loops.
- **Reclaim cadence** — `reclaim_interval` defaults to 30s. May need to drop to 5–10s if Week 3's retrieval reveals stalled jobs delaying read freshness.
- **Job dead-letter UI** — `Failed` jobs sit in `kb_jobs_by_id` for inspection but Week 2 ships no UI / CLI to list them. Week 4 CLI adds `rsclaw kb jobs list --status failed`.

---

## Self-review checklist (run before committing the plan)

- [ ] Every task references **specific** files + line counts; no "see Task X" cross-references that hide the code.
- [ ] No placeholders (TBD / TODO / "implement later") in any code block.
- [ ] Type names match across tasks: `KbStore`, `HandlerCtx`, `WorkerConfig`, `IngestInput`, `IngestOutput` referenced consistently.
- [ ] `chunk_embed` handler is genuinely idempotent (deterministic `chunk_id` from Week 1 + deterministic `StubEmbedder` = same chunks + vectors on rerun).
- [ ] Atomic pipeline: every record written inside `ingest_canonicalized` is in **one** `wtx`; no partial-commit possible.
- [ ] All redb accessors take `&WriteTransaction` (writes) or `&ReadTransaction` (reads) — never both, never neither.
- [ ] Worker pool's `shutdown()` actually drops the background tasks; tests verify by running `run_one_blocking` after shutdown returns nothing.
- [ ] Spec §J flow steps 1–11 each map to a concrete task or are explicitly deferred to Week 3/4.
