# `src/kb/` — Knowledge Base

User-managed RAG knowledge base. See `docs/specs/2026-05-19-knowledge-base.md`
for the full design and `docs/adr/0001-knowledge-base.md` for the decision
record. Week 1 implementation plan lives at
`docs/plans/2026-05-19-kb-mvp-week1-foundation.md`.

## What's implemented (Week 1)

- **Types** (`model/`): KbDoc / KbChunk (with `logical_source_id` for
  idempotency) / KbEntity / KbEntityIndex / LogicalSourceId / KbLocator /
  KbVisibility / CallerScope / VersionPointer / SimHash-64.
- **Content store** (`content_store/`): atomic no-clobber markdown
  writes (via `tempfile::NamedTempFile::persist_noclobber`) under
  `~/.rsclaw/kb/md/<kind>/<slug>--<lsid8>.md` with YAML front-matter.
  The `--<lsid8>` suffix is the first 8 hex chars of
  `sha256(logical_source_id)`, making re-ingest idempotent and
  same-slug-different-source ingests collision-free.
  `stage_doc` verifies body-sha on write_if_new=false collisions and
  errors loudly rather than silently returning a dangling SHA.
  Optional raw bytes go under `~/.rsclaw/kb/raw/<doc_id>.<ext>`;
  `read_doc_range` gives lazy chunk-body retrieval.
- **redb schema** (`store/`): all 13 tables defined and openable
  (`kb_docs` / `kb_doc_latest_version` / `kb_chunks` /
  `kb_chunk_by_logical` / `kb_entities` / `kb_entity_index` /
  `kb_seen_items` / `kb_sync_state` / `kb_ledger` / `kb_jobs_by_id` /
  `kb_jobs_by_dedupe_active` / `kb_jobs_by_status_priority` /
  `kb_job_claims`).
- **Ledger/Jobs types** (`ledger/`, `jobs/`): structs and enums for
  the IngestLedger + Outbox pattern; accessors come in Week 2.
- **Canonicalizers** (`canonicalize/`): markdown, plain text, HTML
  (via lol-html), PDF text layer (no OCR), URL **identity**
  canonicalization (tracker stripping, param sorting — string only;
  HTTP fetch + HTML→Markdown is Week 2).
- **Chunker** (`chunker/`): paragraph splitter with `heading_path`
  injection into `indexed_text`; chunk_ids derived from
  `logical_source_id` for re-ingest idempotency; SimHash-64 near-dup
  dedup.
- **Façade** (`mod.rs`): re-exports the surface most callers need
  (`stage_doc`, `chunk_markdown`, `canonicalize_by_mime`, `KbPaths`,
  …).

## What's NOT in Week 1

- redb accessors (read/write of KbDoc / KbChunk / Ledger / Jobs) → Week 2
- Embedder (BGE-M3 local) → Week 2
- Worker pool + chunk+embed pipeline → Week 2
- URL fetch + HTML→Markdown (`UrlCanonicalizer`) → Week 2
- Tantivy `add_document` + HNSW `insert` → Week 3
- Hybrid retrieval (RRF + MMR) / `kb_search` tool → Week 3
- Visibility filter wiring into retrieval → Week 3
- `ManualUploadSyncer` + `UrlSyncer` + CLI → Week 4
- Compactor → Week 4

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

## Quick start (Week 1 only)

```rust
use rsclaw::kb::{
    canonicalize_by_mime, chunk_markdown, detect_mime, stage_doc,
    CanonicalizeInput, ChunkerInput, FrontMatter, KbPaths, LocatorKind,
    StageInput,
};
use ulid::Ulid;

let paths = KbPaths::new("/path/to/.rsclaw/kb");
paths.ensure_layout()?;

let bytes = std::fs::read("manual.md")?;
let mime = detect_mime(&bytes, Some("manual.md"));
let canon = canonicalize_by_mime(CanonicalizeInput {
    bytes: &bytes,
    mime: &mime,
    hint_title: Some("manual.md"),
    logical_source_id_seed: None,
})?
.unwrap();

let doc_id = Ulid::new().to_string();
let _staged = stage_doc(&paths, StageInput {
    doc_id: &doc_id,
    kind: canon.metadata.source_kind,
    slug: "manual.md",
    logical_source_id: canon.metadata.logical_source_id.as_str(),
    front: FrontMatter {
        title: canon.metadata.title.clone(),
        source_kind: canon.metadata.source_kind.as_str().to_string(),
        logical_source_id: canon.metadata.logical_source_id.as_str().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        tags: canon.metadata.tags.clone(),
        meta: canon.metadata.extra.clone(),
    },
    body: &canon.markdown,
    raw: Some((&bytes, "md")),
    keep_raw: true,
})?;

let chunks = chunk_markdown(ChunkerInput {
    logical_source_id: &canon.metadata.logical_source_id,
    doc_id: &doc_id,
    doc_version: 1,
    markdown_body: &canon.markdown,
    default_locator_kind: LocatorKind::MdSection,
});

// Week 1 stops here — DB writes, embedding, FTS indexing, retrieval
// land in Week 2/3.
```

See `tests/kb_week1_e2e.rs` for the full file → canonicalize → stage
→ chunk flow over `.md`, `.html`, `.txt` fixtures.

## Testing

```bash
cargo test -p rsclaw --lib kb::          # unit tests (~115)
cargo test --test kb_week1_e2e           # integration tests (6)
```
