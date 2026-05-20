# Review — `worktree-feat+knowledge-base` (Knowledge Base MVP)

Reviewer pass: 2026-05-21. Scope: 131 commits vs merge-base `240bae7`,
~28k insertions / 114 files. New `src/kb/` module (RAG: canonicalize →
ledger → outbox worker → HNSW + tantivy + RRF/MMR), `src/embed/`
extraction, `/api/v1/knowledge/*` HTTP API, `rsclaw kb` CLI, 5 `kb_*`
tools.

Tags per AGENTS.md: `[BLOCK]` must-fix · `[SUGGEST]` recommended ·
`[NOTE]` observation.

---

## Overall

Strong, well-structured work. The queue is redb-native with atomic
single-write-transaction claims and fencing tokens; search output is
deterministic for KV-cache friendliness; visibility filtering is
enforced at the retrieval layer; the `/api/v1/knowledge/*` routes sit
behind the global `auth_middleware` (no bypass); the embedder refactor
into `crate::embed` is surgical with backward-compat re-exports. Test
coverage is broad (in-process HTTP e2e, recovery, compactor, CLI smoke).

The blocking items below are all in the **crash / failure path**, not
the happy path — but two of them defeat mechanisms the code claims to
have.

---

## [BLOCK] 1 — `reclaim_stale` is never called in production (confidence 9/10)

`src/kb/store/jobs.rs:173`, `src/kb/worker/pool.rs:106-153`,
`src/gateway/startup.rs:722`

The gateway starts indexing via `knowledge_svc.spawn_worker()`
(`service.rs:186`), a `std::thread` loop that calls `drain_once()` →
`WorkerPool::run_one_blocking()`. `run_one_blocking` only does
`claim_next → handle → mark`. It never calls `reclaim_stale`.

The reclaim loop (`claim_ttl_ms`, `expires_at`, `reclaim_stale`,
`run_reclaim_once`) lives only in `WorkerPool::run_main`, reached via
`WorkerPool::start` — which is used **only in tests** (`pool.rs:255`).
Grep confirms zero production callers of `start` / `reclaim_stale`.

Consequence: if the process dies while a `ChunkAndEmbed` job is
`Running`, the restarted worker's `claim_next` only scans `Ready` jobs,
so the stranded `Running` job is never recovered. Its dedupe key
remains, so re-uploading the same content dedupes back to the dead job
(`enqueue` at `jobs.rs:23`). The document stays `pending` forever with
no recovery short of manual DB surgery.

This also contradicts the README (invariant 31, lines 32/214/318),
which states `reclaim_stale` runs "every `reclaim_interval`." The
shipping gateway has no such loop. Tests pass because they exercise
`WorkerPool::start`; production doesn't use it.

Fix options: (a) have `spawn_worker` drive `WorkerPool::start` (tokio,
already has the reclaim loop), or (b) call `jobs::reclaim_stale` on a
timer inside the `std::thread` loop.

## [BLOCK] 2 — production worker has no panic isolation (confidence 9/10)

`src/kb/service.rs:194-205`

The `spawn_worker` loop handles `Ok(true)` / `Ok(false)` / `Err`, but a
**panic** inside `drain_once` (chunking, embedding, tantivy, hnsw)
unwinds out of the closure and terminates the worker thread. There is no
restart and no `catch_unwind`. After that, every upload sits `pending`
forever.

This is not theoretical: commit `047b8b4` ("fix(kb): CJK tokenizer panic
on overlapping jieba segments") is a panic that already occurred on this
exact path. Combined with [BLOCK] 1, one poison document both kills the
worker and strands its `Running` job.

Fix: wrap the per-job handler in `std::panic::catch_unwind` and, on
panic, mark the job failed (or requeue toward `max_attempts`) so the
loop survives. The fencing already lets you `mark_failed` safely.

---

## [SUGGEST] 3 — KB store open is unconditional and fatal to the whole gateway

`src/gateway/startup.rs:718-722`

`KnowledgeService::open(...).expect("open knowledge base store")` runs
on every startup regardless of whether KB is configured, and a worker is
spawned unconditionally. A corrupt/locked redb, bad permissions, or full
disk panics the entire gateway — taking down all 13 channels and
providers — for an optional RAG feature.

`.expect()` with a message is allowed by AGENTS, and this matches the
sibling A2A store at `:709`. But KB is more optional than core/A2A
state. Consider: open lazily / only when `config.kb` is present, and on
failure log + run without KB rather than aborting boot.

## [SUGGEST] 4 — silent discard of `remove_file` error in compactor

`src/kb/compactor/mod.rs:56` — `let _ = std::fs::remove_file(&abs);`

AGENTS lists `let _ = ...` as an auto-BLOCK trigger; the rule is "if
best-effort, log with `warn!`." This drops a real fallible IO result
silently. A persistently failing unlink (permissions) would leak orphan
files with no signal. Log on `Err`. (The other `let _ =` sites —
`schema.rs` table-handle discards, `write!`-to-`String`, the
`events.send` broadcast with no subscribers — are fine.)

---

## [NOTE] 5 — unchecked index into embedder output

`src/kb/search/pipeline.rs:84` — `self.index.hnsw.search(&qv[0], ...)`.
`qv` is `embed_batch(&[dense_query])`; if an embedder ever returns an
empty vec, `qv[0]` panics the request. Defensive `qv.first()` →
`bail!` is cheap insurance for a 1-line change.

## [NOTE] 6 — `MAX_DOC_BYTES` hardcoded

`src/server/knowledge.rs:31` pins 50 MB; the spec references
`knowledge.maxDocMb`. Comment already flags it as "could be made
configurable later." Wire it to config when convenient.

## [NOTE] 7 — reclaim path requeues without `max_attempts` check

`src/kb/store/jobs.rs:185-194` — `reclaim_stale → requeue` increments
`attempts` but never compares to `max_attempts`. A job whose worker
repeatedly dies would requeue unboundedly. Moot today (reclaim isn't run
in prod — see [BLOCK] 1) but fix it together with that item.

---

## Resolution — all items fixed (2026-05-21)

- **[BLOCK] 1** — `reclaim_stale` now runs in production: `KnowledgeService::spawn_worker`
  sweeps every 30s via the new `reclaim_stale()` method (`service.rs`). README
  invariant 13 updated.
- **[BLOCK] 2** — `run_one_blocking` wraps the handler in `catch_unwind`; a panic
  becomes an `Err` and goes through the normal requeue/fail path. New test
  `handler_panic_is_isolated_and_fails_job`.
- **[SUGGEST] 3** — `AppState.knowledge` is now `Option`; startup logs and
  continues if the KB store fails to open, and `/knowledge` routes mount only
  when present (`startup.rs`, `server/mod.rs`).
- **[SUGGEST] 4** — compactor warn-logs `remove_file` failures.
- **[NOTE] 5** — `pipeline.rs` uses `qv.first()` (no index panic).
- **[NOTE] 6** — `kb.maxDocMb` config added and threaded into the upload
  body limit (`routes(max_doc_bytes)`, `KnowledgeService::max_doc_bytes()`).
- **[NOTE] 7** — `reclaim_stale` takes `max_attempts` and fails out poison jobs
  instead of requeueing forever. New test `reclaim_stale_fails_job_past_max_attempts`.

Also fixed latent test-compilation breakage the branch had introduced (the
new `AppState.knowledge` field and `KbSearchInput.query_instruction` field were
never added to `tests/common/mod.rs`, `gateway_health.rs`, `agent_turn.rs`,
`kb_week3_search.rs`, `kb_entities_e2e.rs` — so those test binaries didn't
compile). Now they do.

Verification: 246 kb lib unit tests pass; `kb_week2_recovery`, `kb_week3_search`,
`kb_entities_e2e`, and `server::knowledge` HTTP tests pass; `gateway_health` +
`agent_turn` compile. (Full-suite run was blocked only by a 100%-full disk; the
worktree's regenerable incremental cache was cleared to make room.)

## Verified clean

- Queue atomicity: redb single-writer + single-write-transaction claim;
  fencing token in `verify_claim_token` correctly rejects stale/foreign
  completions (`jobs.rs:141`). Tests `mark_done_with_wrong_token_errors`,
  `mark_done_after_reclaim_errors` cover it.
- Auth: `/api/v1/knowledge/*` is under the global `auth_middleware`
  (`server/mod.rs:353`), not in any bypass list.
- Visibility: `keep_doc(&d, scope, filter)` enforced before fusion;
  `search_filter_by_visibility_hides_private` proves cross-user isolation.
- Determinism: post-MMR `(score desc, chunk_id asc)` re-sort gives stable
  on-the-wire order (`pipeline.rs:208`), tested.
- Graceful degradation: chunk body-read failure logs + degrades to empty
  text instead of erroring the whole search (`pipeline.rs:226`).
- Embedder refactor (`agent/memory.rs` → `crate::embed`) is a clean
  extraction with backward-compat re-exports.
