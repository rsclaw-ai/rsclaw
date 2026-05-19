# `src/kb/` — Knowledge Base

User-managed RAG knowledge base. See `docs/specs/2026-05-19-knowledge-base.md`
for the full design and `docs/adr/0001-knowledge-base.md` for the decision
record. Week 1 plan: `docs/plans/2026-05-19-kb-mvp-week1-foundation.md`.
Week 2 plan: `docs/plans/2026-05-19-kb-mvp-week2-pipeline.md`.

## What's implemented (Weeks 1–2)

**Week 1 (Foundation):**

- Types, content store, canonicalizers, chunker, redb schema, file IO primitives.

**Week 2 (Persistence + Pipeline):**

- **redb accessors** (`store/docs`, `store/chunks`, `store/seen`,
  `store/ledger`, `store/jobs`) — composable inside a single
  `WriteTransaction` so the pipeline can write doc + ledger + job +
  seen atomically. Each table has `*_in_wtx` reader variants for the
  race-safe NOOP re-check inside the ingest pipeline.
- **`KbStore` facade** — owns the `redb::Database`, exposes
  `begin_write` / `begin_read`.
- **`KbEmbedder` trait + `StubEmbedder`** — deterministic 1024-dim
  vectors for tests; real BGE-M3 embedder lands as a self-contained
  follow-up behind the same trait.
- **`ingest_canonicalized()`** — single-tx atomic pipeline. Fast-path
  NOOP read, file staging, then one `WriteTransaction` does the race-safe
  NOOP re-check + version compute + 5-table write + commit. Returns
  `doc_id` synchronously.
- **`WorkerPool`** — single tokio task that claims `Ready` jobs from
  `kb_jobs_by_status_priority`, dispatches to `JobHandler`, marks
  `Done` / `Failed` / requeues. `reclaim_stale` interleaved every
  `reclaim_interval` for expired claims. `mark_done` / `mark_failed`
  verify the claim's fencing token so zombie workers can't clobber
  the new claimant's state. Requires multi-threaded tokio runtime
  (uses `tokio::task::block_in_place`).
- **`ChunkAndEmbed` handler** — reads staged markdown, runs the
  Week 1 chunker, embeds via `KbEmbedder`, writes chunks + advances
  ledger to `IndexingComplete`. Idempotent on rerun (deterministic
  `chunk_id`); drops stale chunks from prior `doc_version`s before
  inserting the new set.
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

## Architecture invariants (verify after every code change)

1. **`chunk_id` depends on `logical_source_id`, never on `doc_id` or
   `doc_version`**: re-ingesting the same file produces identical
   `chunk_id`s. Covered by
   `kb::model::chunk::tests::reingest_same_file_same_chunk_ids`,
   `kb::chunker::tests::idempotent_chunk_ids`, and
   `tests/kb_week1_e2e.rs::reingest_same_file_same_chunk_ids`.
2. **`KbDoc.visible_to(scope)` is the only visibility entry point**:
   never call `KbVisibility::visible_to(scope, owner)` directly from
   retrieval code — pairing the wrong owner is the most likely
   scope-leak. Covered by
   `kb::model::doc::tests::visibility_private_requires_matching_owner`
   and `kbdoc_visible_to_pairs_owner_with_visibility`.
3. **`write_if_new` is truly atomic no-clobber**: never replace it
   with `path.exists()` + `rename()` — that's a TOCTOU race AND Unix
   `rename(2)` overwrites. Covered by
   `kb::content_store::atomic::tests::write_if_new_concurrent_no_clobber`
   (20-iteration thread race).
4. **`stage_doc` errors on divergent body at the same path**: a
   `write_if_new=false` with a different body means either (a) a
   32-bit `lsid_hash8` collision or (b) a non-deterministic
   canonicalizer — both must surface immediately, not silently. Covered
   by `kb::content_store::tests::stage_collision_with_divergent_body_errors`.
5. **Files are stage-only**: nothing in `canonicalize/` or
   `content_store/` deletes files. Deletion happens via the compactor
   + ledger reconciliation in Week 4.
6. **No SQL pretense**: redb queries are KV / range-scan only; never
   use SQL terminology (no "partial unique index", no "UPDATE …
   RETURNING").
7. **PII in logs goes through `util::redact`**: source ids and
   content previews emit only `redact(s)` (first 8 hex of sha256).

### Added in Week 2

8. **All ingest writes happen in one redb tx** — `ingest_canonicalized`
   commits `KbDoc` + `VersionPointer` + `IngestLedgerEntry` + `Job` +
   `SeenItems` together. Splitting any of these into separate txs
   reintroduces the Outbox bug: a doc visible to readers but no job
   queued for chunking. Covered by
   `kb::pipeline::ingest::tests::fresh_ingest_writes_all_tables`.
9. **NOOP re-check + version compute happen INSIDE the wtx** — these
   reads use `*_in_wtx` accessor variants so a concurrent ingest with
   the same `(lsid, raw_sha)` cannot pass NOOP-miss in both threads and
   produce duplicate docs. redb's single-writer guarantee plus the
   in-wtx re-check is the correctness hinge. Covered by
   `kb::pipeline::ingest::tests::concurrent_ingest_same_bytes_produces_one_doc`.
10. **`ChunkAndEmbed` handler is idempotent** — re-running on the same
    `doc_id` produces identical chunks (deterministic `chunk_id`) and
    identical vectors. Re-runs after the ledger already advanced are
    safe no-ops, not errors. Covered by
    `kb::worker::handlers::chunk_embed::tests::idempotent_rerun_produces_same_chunks`
    and `rerun_after_ledger_advanced_does_not_error`.
11. **Job dedupe is keyed on `JobKind::dedupe_key()`, not job_id** —
    enqueueing the same logical work twice while a job is `Ready` or
    `Running` returns the existing `job_id` without writing a duplicate.
    Covered by `kb::store::jobs::tests::enqueue_dedupes_active_jobs`.
12. **`mark_done` / `mark_failed` verify the claim's fencing token** —
    a zombie worker whose claim was reclaimed cannot transition the
    job and clobber the new claimant. Covered by
    `kb::store::jobs::tests::mark_done_with_wrong_token_errors` and
    `mark_done_after_reclaim_errors`.
13. **Stalled claims auto-reclaim** — workers that crash mid-job leave
    a claim with `expires_at` in the past; the next `reclaim_stale`
    sweep resets the job to `Ready` and another worker re-runs it.
    Covered by
    `tests/kb_week2_recovery.rs::stalled_claim_is_reclaimed_and_rerun`.
14. **`WorkerPool::shutdown()` exits in bounded time** — the AtomicBool
    is checked at the top of each loop iteration and on every wake
    from the idle sleep. Long-running handlers delay shutdown only
    until they return. Covered by
    `kb::worker::pool::tests::shutdown_exits_within_poll_idle_plus_margin`.

## Quick start (Weeks 1–2)

```rust
use rsclaw::kb::{
    canonicalize_by_mime, detect_mime, ingest_canonicalized,
    CanonicalizeInput, HandlerCtx, IngestInput, KbEmbedder, KbPaths, KbStore,
    StubEmbedder, WorkerConfig, WorkerPool,
};
use std::sync::Arc;

# async fn demo() -> anyhow::Result<()> {
let tmp = tempfile::TempDir::new()?;
let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
paths.ensure_layout()?;
let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

// Start the worker pool (requires multi-threaded tokio runtime).
let ctx = HandlerCtx {
    store: store.clone(),
    paths: paths.clone(),
    embedder,
};
let pool = WorkerPool::start(ctx, WorkerConfig::default());

// Ingest a doc.
let bytes = std::fs::read("manual.md")?;
let mime = detect_mime(&bytes, Some("manual.md"));
let canon = canonicalize_by_mime(CanonicalizeInput {
    bytes: &bytes,
    mime: &mime,
    hint_title: Some("manual.md"),
    logical_source_id_seed: None,
})?
.unwrap();

let out = ingest_canonicalized(
    &store,
    IngestInput {
        canon: &canon,
        raw_bytes: &bytes,
        raw_ext: "md",
        visibility: None,
        owner_user_id: None,
        seen_key: None,
        source: None,
        paths: &paths,
    },
)?;
println!("doc_id: {}", out.doc_id);

// Worker pool picks up the ChunkAndEmbed job asynchronously and
// writes chunks + vectors into kb_chunks. See
// `tests/kb_week2_pipeline.rs` for the full async wait pattern.

pool.shutdown().await;
# Ok(()) }
```

## Testing

```bash
cargo test -p rsclaw --lib kb::          # unit tests (~160)
cargo test --test kb_week1_e2e           # integration tests (6)
cargo test --test kb_week2_pipeline      # async e2e (1)
cargo test --test kb_week2_recovery      # crash recovery (2)
```
