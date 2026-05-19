# `src/kb/` — Knowledge Base

User-managed RAG knowledge base. See `docs/specs/2026-05-19-knowledge-base.md`
for the full design and `docs/adr/0001-knowledge-base.md` for the decision
record. Week 1 plan: `docs/plans/2026-05-19-kb-mvp-week1-foundation.md`.
Week 2 plan: `docs/plans/2026-05-19-kb-mvp-week2-pipeline.md`.

## What's implemented (Weeks 1–4)

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

**Week 3 (Retrieval):**

- **HnswCache** (`index/hnsw.rs`) — `RwLock<Hnsw<f32, DistCosine>>`
  with rebuild from redb on startup. Append-only `insert` (re-inserting
  an id orphans the old vertex; compactor reaps via rebuild).
- **TantivyIndex** (`index/tantivy.rs`) — BM25 with `chunk_id`-keyed
  delete-then-add upsert + `delete_all_documents` for full rebuild.
  Uses the `JiebaTokenizer` (`index/cjk.rs`) so Chinese queries
  match against jieba-segmented tokens; ASCII queries round-trip
  identically.
- **KbIndex composite** (`index/mod.rs`) — single handle wraps both
  layers; `upsert_chunk` + `commit` are the worker write path.
- **Worker integration** — `ChunkAndEmbed` handler writes to both
  indexes after the redb commit lands; failures propagate so the
  worker retries (chunks are already durable in redb).
- **Filter** (`search/filter.rs`) — visibility + status + version +
  tags + source_kind + doc_ids. Single source of truth for "can this
  caller see this hit".
- **RRF + MMR** (`search/rrf.rs`, `search/mmr.rs`) — pure-function
  fusion + diversity selector.
- **Pipeline** (`search/pipeline.rs`) — `SearchCtx::search` composes
  dense + sparse → filter → fuse → MMR → lazy text fetch.
- **Tools** (`tools/`) — `kb_search`, `kb_fetch`, `kb_list_docs`,
  `kb_similar`, `kb_search_entities`. JSON-shaped IO; `CallerScope`
  is a separate function arg (runtime-injected, agent cannot supply).
- **Entity store** (`store/entities.rs`) — put_entity + get_entity +
  find_by_surface scan + chunks_for_entity edges. Inverted index is a
  Week 4 optimisation once entity extraction emits non-trivial counts.

**Week 4 (Syncers + Compactor + CLI):**

- **`KbSourceSyncer` trait** (`sync/mod.rs`) — generic interface for
  source-specific ingest. Async via `async-trait`.
- **`ManualUploadSyncer`** (`sync/manual.rs`) — file path → bytes →
  `canonicalize_by_mime` → `ingest_canonicalized`. Used by
  `rsclaw kb add <path>`.
- **`UrlSyncer`** (`sync/url.rs`) — `reqwest::get` with
  ETag/Last-Modified conditional headers; falls back to content-hash
  dedupe via `seen_items`. Cursor persisted as
  `etag:` / `lastmod:` / `contenthash:` in `SyncState`.
- **`SyncRegistry`** (`sync/state.rs`) — load/save SyncState per
  source_id wrapper around `store::seen::{get,put}_sync_state`.
- **Compactor** (`compactor/mod.rs`) — orphan file scan with
  grace-period guard + ledger advancement
  `IndexingComplete → CleanupPending → Done`. Single
  `run_compactor_tick` function, idempotent.
- **CLI** (`src/cli/kb.rs` + `src/cmd/kb.rs`) — `rsclaw kb add | ls |
  rm | search | show | visibility | compact | stats`. `kb add`
  synchronously drains the worker pool so follow-up `kb search`
  sees fresh chunks immediately.

**Week 5 (Polish):**

- **CJK tokenizer** (`index/cjk.rs`) — `JiebaTokenizer` registered
  as tantivy's `cjk` analyzer. The schema applies it to
  `indexed_text` so Chinese BM25 queries actually match.
- **Regex entity extractor** (`entities/extract.rs`) — pulls URLs /
  emails / hashtags / @-mentions out of each chunk's
  `indexed_text`. The worker handler upserts `KbEntity` +
  `KbEntityIndex` edges per chunk, activating
  `kb_search_entities`. CJK hashtags supported.

## What's NOT in Weeks 1–4

- BGE-M3 embedder (real model) — Week 2.5 (self-contained behind `KbEmbedder` trait)
- HNSW snapshot persistence — Week 5.5 (rebuild from redb on startup today)
- BGE-M3 real embedder — Week 2.5 (StubEmbedder today)
- Entity boosting in retrieval — Week 5.5 (`boost_entities` /
  `require_entities` filters currently no-op; entity edges now exist
  so a Week 5.5 task wires them into the search pipeline)
- Scheduled syncer ticks (cron integration) — Week 5.5
- LocalFolderSyncer, MailSyncer, ChatSyncer — V2 (post-MVP)
- `kb_explain` retrieval trace — V2 (post-MVP)
- Tauri admin UI — V2 (post-MVP)
- ML-based NER (replaces regex extractor) — V2 (post-MVP)

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
4. **Markdown paths are content-addressed**: layout is
   `md/<kind>/<slug>--<lsid8>--<md8>.md` where `lsid8` =
   `sha256(logical_source_id)[:8]` and `md8` =
   `sha256(body)[:8]`. Same lsid + same content → same path
   (idempotent re-ingest). Same lsid + new content (v2 ingest under a
   stable seed) → different path; both versions coexist until the
   Week 4 compactor reaps the old file. `stage_doc` still errors on
   any body mismatch at a same-path hit (full 64-bit suffix
   collision, ~2^-32). Covered by
   `kb::content_store::paths::tests::markdown_rel_same_lsid_different_body_different_path`
   and
   `kb::content_store::tests::stage_same_lsid_different_body_lands_at_different_paths`.
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

### Added in Week 3

15. **Visibility filter runs on every retrieval call** — every
    `tools/kb_*` entry point goes through `search::filter::keep_doc` +
    `is_latest_version`. There is no caller-supplied bypass. Covered
    by
    `kb::search::pipeline::tests::search_filter_by_visibility_hides_private`.
16. **HNSW + tantivy are caches over redb** — losing either is a
    rebuild, not data loss. `KbIndex::open_and_rebuild` reconstructs
    both from `kb_chunks` on startup. Covered by
    `kb::index::hnsw::tests::rebuild_then_search_returns_hits` and
    `kb::index::tests::open_and_rebuild_recovers_both_layers`.
17. **Tantivy upsert deletes-by-term before add** — re-running
    `chunk_embed` on the same chunk_id replaces the indexed text
    rather than producing a duplicate match. Covered by
    `kb::index::tantivy::tests::upsert_replaces_previous`.
18. **CallerScope is injected by the runtime, not by tool input** —
    `kb_search::KbSearchInput` deliberately has no `caller_scope`
    field; the runtime constructs scope from auth context and passes
    it as a separate function argument to `tools::*::run`.

### Added in Week 4

19. **All syncers go through `ingest_canonicalized`** — `ManualUpload`
    and `Url` syncers both terminate in `ingest_canonicalized(...)`,
    so spec §J's atomicity contract holds for every ingest path. No
    syncer ever writes to redb directly.
20. **UrlSyncer conditional-get uses SyncState.cursor** — every
    304 NOT_MODIFIED response counts as `docs_skipped`, never
    `docs_added`. Covered by the `manual_syncer_dedupes_identical_bytes`
    pattern (UrlSyncer integration deferred to Week 4.5 with a
    `wiremock` dep).
21. **Compactor never deletes files referenced by any KbDoc** —
    `referenced_paths` unions over every doc's
    `markdown_path` + `raw_path` plus every Pending/IndexingComplete
    ledger entry's `new_paths`. The grace period (default 1h) guards
    against in-flight ingest. Covered by
    `kb::compactor::tests::referenced_file_preserved`.
22. **CLI is a thin wrapper over the library surface** — every
    `rsclaw kb` subcommand calls into Week 2–3's tool surface
    (`ingest_canonicalized`, `kb_search`, `kb_list_docs`, `kb_fetch`)
    or the new Week 4 syncer/compactor functions. `kb add` drains
    the worker pool synchronously so an immediate `kb search` sees
    fresh chunks.

### Added in Week 5 (Polish)

23. **CJK BM25 search works** — `JiebaTokenizer` is registered as
    tantivy's `cjk` analyzer and applied to the `indexed_text`
    field's `TextOptions`. The default whitespace+lowercase
    analyzer reduced Chinese sentences to a single un-searchable
    token; jieba splits them into searchable terms. Covered by
    `kb::index::tantivy::tests::chinese_query_matches_chinese_doc`.
24. **Entity edges land on every chunk write** — the regex
    extractor (`entities/extract.rs`) runs inside the same
    `wtx` as the chunk insert, so `KbEntityIndex` rows are
    consistent with chunks. `kb_search_entities` returns these
    edges; `boost_entities` / `require_entities` filters can be
    wired in Week 5.5 without re-ingest. Covered by
    `tests/kb_entities_e2e.rs::entities_extracted_and_queryable`.

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
cargo test -p rsclaw --lib kb::          # unit tests (~200)
cargo test --test kb_week1_e2e           # Week 1 integration (6)
cargo test --test kb_week2_pipeline      # Week 2 async e2e (1)
cargo test --test kb_week2_recovery      # Week 2 crash recovery (2)
cargo test --test kb_week3_search        # Week 3 retrieval e2e (1)
cargo test --test kb_week4_syncers       # Week 4 syncer e2e (2)
cargo test --test kb_week4_compactor     # Week 4 compactor integration (1)
cargo test --test kb_entities_e2e        # Week 5 entity extraction (1)
```

End-to-end CLI smoke:

```bash
echo "# Hello\n\nThe quick brown fox." > /tmp/doc.md
rsclaw --base-dir /tmp/kbdemo kb add /tmp/doc.md --tags demo
rsclaw --base-dir /tmp/kbdemo kb search "brown fox"
rsclaw --base-dir /tmp/kbdemo kb stats
```
