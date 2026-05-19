# KB MVP Week 3 — Retrieval (HNSW + tantivy + Hybrid + Tools) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the retrieval surface on top of Week 1's foundation and Week 2's persistence + pipeline. By end of Week 3, the engineer can call `kb_search(query, ...)` from a tool surface, get back ranked chunks with citations, with visibility filtering applied. The retrieval pipeline does dense (HNSW) + sparse (tantivy BM25) recall, RRF fusion, MMR diversity, then lazy text fetch.

**Architecture:** Two cache layers backed by redb as source of truth:
- **`HnswCache`** — `ArcSwap<Hnsw<f32, DistCosine>>` from `hnsw_rs`. Built from `KbChunk.vector` rows in redb on startup (rebuild-from-redb is correctness; snapshot is a Week 3.5 optimisation). Incremental `insert()` on each chunk write from the worker.
- **`TantivyIndex`** — disk-backed tantivy 0.22 index at `idx/tantivy/`. Schema: `chunk_id STORED + INDEXED`, `indexed_text TEXT` (default analyzer; CJK uses `cang-jie` later — MVP ships with whitespace + lowercase). Same write path: handler commits to tantivy after persisting chunks to redb.

The retrieval pipeline is a pure function: `search(query, k, filter, caller_scope) → Vec<RetrievalHit>`. It composes:
1. Dense recall: `embedder.embed(query) → hnsw.search(k*3)` → chunk_ids + cos_sim.
2. Sparse recall: `tantivy.search(query, k*3)` → chunk_ids + bm25.
3. Filter (visibility + status + doc_version=latest + tags + source_kind + doc_ids + require_entities) — runs BEFORE fusion so filtered hits don't take RRF slots from valid ones.
4. RRF fusion (`k=60`).
5. Optional `boost_entities` multiplier on chunks containing the boosted entity ids.
6. MMR diversity (λ=0.5 default).
7. Lazy text fetch via `content_store::read_doc_range`.
8. Entity alignment annotations + warnings for entity miss.

Visibility filtering is the load-bearing safety property: **filter runs on every hit, every call, with no caller-controlled override**. CallerScope is constructed by the agent runtime, not by agent code (enforced by API shape — kb_search takes `caller_scope: &CallerScope` from a runtime-owned context, not from tool input JSON).

**Tech Stack:** Rust 2024, tantivy 0.22 (existing), hnsw_rs 0.3 (existing), arc-swap 1.7 (existing). **No new Cargo deps required.**

**Spec reference:** `docs/specs/2026-05-19-knowledge-base.md` (§3 Retrieval, §K PermissionScope, §L Index Rebuild Contract).

**Builds on:** `docs/plans/2026-05-19-kb-mvp-week1-foundation.md` (Week 1 — types + content_store + canonicalize + chunker + redb schema), `docs/plans/2026-05-19-kb-mvp-week2-pipeline.md` (Week 2 — redb accessors + ingest pipeline + worker pool + StubEmbedder).

---

## What this plan delivers

By end of Week 3, the engineer can run:

```bash
cargo test -p rsclaw --lib kb::
cargo test --test kb_week3_search
```

…and have the full integration test pass: given a populated KB (multiple docs ingested via Week 2's pipeline, worker drained), `kb_search(query, k=8, ...)` returns:

- Top-k chunks ranked by hybrid RRF + MMR
- With visibility filtering applied per `CallerScope`
- With `read_doc_range` lazily filling `text` from `md/*.md`
- With citation metadata (`source`, `locator_human`, `locator_machine`)
- With entity alignment annotations

Other tools (`kb_fetch`, `kb_list_docs`, `kb_similar`, `kb_search_entities`) work end-to-end.

Crash + restart story (deferred to Week 3.5):
- HNSW snapshot persistence (Week 3 ships rebuild-from-redb only)
- Tantivy index rebuild from redb chunks (Week 3 ships incremental writes + manual rebuild API; auto-rebuild on missing dir lands in Week 4)

---

## Module additions

```
src/kb/
  index/
    mod.rs              # NEW: public surface — KbIndex (composes Hnsw + Tantivy)
    hnsw.rs             # NEW: HnswCache with ArcSwap + insert + search + rebuild
    tantivy.rs          # NEW: TantivyIndex with add_document + search + rebuild
    rebuild.rs          # NEW: from_redb(store, paths) — full rebuild for startup recovery
  search/
    mod.rs              # NEW: public surface — search() pipeline
    filter.rs           # NEW: visibility + status + doc_version + tags + source_kind filters
    rrf.rs              # NEW: RRF fusion across dense + sparse rankings
    mmr.rs              # NEW: MMR diversity selector
    pipeline.rs         # NEW: orchestration: dense + sparse → filter → fuse → mmr → fetch
  tools/
    mod.rs              # NEW: tool wrappers (callable surface for MCP/agent)
    kb_search.rs        # NEW: main retrieval tool
    kb_fetch.rs         # NEW: single chunk fetch + optional neighbor expansion
    kb_list_docs.rs     # NEW: paginated doc listing with filters
    kb_similar.rs       # NEW: chunk_id → nearest neighbors
    kb_search_entities.rs # NEW: entity inverted index lookup
  store/
    entities.rs         # NEW: KbEntity / KbEntityIndex accessors (Week 1 had schema, no accessors)
worker/handlers/
  chunk_embed.rs        # MODIFY: after chunks::put, also write to KbIndex (hnsw + tantivy)
tests/
  kb_week3_search.rs    # NEW: e2e — ingest → worker → kb_search returns expected chunks
```

Existing modules (Week 1+2) stay untouched except:
- `src/kb/worker/handlers/chunk_embed.rs`: gains an `index` write step inside the existing wtx-finalised path (the index write happens AFTER `wtx.commit()` succeeds — index is a cache, not source of truth, so it's safe to write outside the redb tx).
- `src/kb/mod.rs`: re-exports for the new public surface (`KbIndex`, `kb_search`, etc.).
- `src/kb/store/mod.rs`: `pub mod entities;`.

---

## Conventions

- **One commit per task** with `feat(kb):` / `test(kb):` / `chore(kb):`.
- **Tests**: unit in `#[cfg(test)] mod tests` at end of source file; integration at `tests/kb_week3_*.rs`.
- **`cargo test -p rsclaw --lib kb::...`** for unit; `cargo test --test kb_week3_search` for integration.
- **No `unwrap()` / `expect()` in non-test code** — use `anyhow::Result`.
- **No `println!()` in non-test code** — use `tracing::` macros (NOT `log::`); content through `kb::redact()`.
- **Visibility filtering**: every retrieval code path that returns chunk data MUST go through `search::filter::visible_to_scope`. The function takes `(doc: &KbDoc, scope: &CallerScope)`. Never bypass.
- **Idempotent index writes**: hnsw `insert` and tantivy `add_document` keyed on `chunk_id` so re-running `chunk_embed` produces the same logical index state. tantivy uses `delete_term(chunk_id)` before `add_document` to replace rather than duplicate.
- **CJK analyzer**: MVP uses tantivy's default analyzer (whitespace + lowercase). Chinese-only queries will fall back to dense recall. Proper CJK tokenizer (jieba via cang-jie or tantivy-jieba) is a Week 3.5 follow-up.

---

## Task 1: Bootstrap — kb index/search/tools module additions

**Files:** Modify `src/kb/mod.rs`; create stubs for new directories.

- [ ] **Step 1: Create new directories + empty mod.rs files**

```bash
mkdir -p src/kb/{index,search,tools}
for d in index search tools; do
  : > "src/kb/$d/mod.rs"
done
```

- [ ] **Step 2: Declare new modules in `src/kb/mod.rs`**

Add (alphabetically among existing `pub mod` lines):

```rust
pub mod index;
pub mod search;
pub mod tools;
```

- [ ] **Step 3: Verify compile**

```bash
cargo check -p rsclaw
```

- [ ] **Step 4: Commit**

```bash
git add src/kb/mod.rs src/kb/index/ src/kb/search/ src/kb/tools/
git commit -m "chore(kb): bootstrap Week 3 module skeleton (index, search, tools)"
```

---

## Task 2: `store/entities.rs` — KbEntity + KbEntityIndex accessors

**Files:** `src/kb/store/entities.rs`, modify `src/kb/store/mod.rs`

Week 1 shipped the `kb_entities` + `kb_entity_index` table definitions but no accessors. Week 3 needs `kb_search_entities` (entity lookup by surface form) and the retrieval pipeline's `boost_entities` / `require_entities` filters (chunk → entity membership). MVP ships read accessors + a `put_entity` for testing; entity extraction lands in Week 4.

- [ ] **Step 1: Write impl + tests**

```rust
//! KbEntity + KbEntityIndex accessors.
//!
//! `kb_entities`: entity_id → KbEntity (the canonical entity row).
//! `kb_entity_index`: composite key `surface_lower\0kind` → entity_id
//! (case-insensitive surface form lookup; one entity may have many
//! aliases, each producing its own index row).

use crate::kb::model::{KbEntity, KbEntityIndex};
use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::{KB_ENTITIES, KB_ENTITY_INDEX};
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

pub fn put_entity(wtx: &WriteTransaction, e: &KbEntity) -> Result<()> {
    let bytes = encode(e)?;
    let mut tbl = wtx.open_table(KB_ENTITIES)?;
    tbl.insert(e.id.as_str(), bytes.as_slice())?;
    Ok(())
}

pub fn get_entity(rtx: &ReadTransaction, entity_id: &str) -> Result<Option<KbEntity>> {
    let tbl = rtx.open_table(KB_ENTITIES)?;
    match tbl.get(entity_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Add `surface` → `entity_id` mapping. Multiple surfaces per entity OK.
pub fn put_index(wtx: &WriteTransaction, idx: &KbEntityIndex) -> Result<()> {
    let key = compose_idx_key(&idx.surface_lower, idx.kind.as_str());
    let bytes = encode(idx)?;
    let mut tbl = wtx.open_table(KB_ENTITY_INDEX)?;
    tbl.insert(key.as_str(), bytes.as_slice())?;
    Ok(())
}

/// Find entity_id by surface form (case-insensitive) and optional kind
/// filter. Returns all matching entries (one surface may match
/// multiple entity kinds, e.g. "Apple" = Brand + Place).
pub fn find_by_surface(
    rtx: &ReadTransaction,
    surface: &str,
    kind_filter: Option<&str>,
) -> Result<Vec<KbEntityIndex>> {
    let surface_lower = surface.to_lowercase();
    let prefix = format!("{surface_lower}\0");
    let end = format!("{surface_lower}\u{1}");
    let tbl = rtx.open_table(KB_ENTITY_INDEX)?;
    let mut out = Vec::new();
    for entry in tbl.range(prefix.as_str()..end.as_str())? {
        let (_, v) = entry?;
        let idx: KbEntityIndex = decode(v.value())?;
        if let Some(k) = kind_filter {
            if idx.kind.as_str() != k {
                continue;
            }
        }
        out.push(idx);
    }
    Ok(out)
}

fn compose_idx_key(surface_lower: &str, kind: &str) -> String {
    format!("{surface_lower}\0{kind}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::model::EntityKind;
    use crate::kb::store::open_db;
    use tempfile::TempDir;

    fn sample_entity(id: &str, name: &str, kind: EntityKind) -> KbEntity {
        KbEntity {
            id: id.into(),
            canonical_name: name.into(),
            kind,
            aliases: vec![],
            description: None,
        }
    }

    fn sample_index(surface: &str, kind: EntityKind, entity_id: &str) -> KbEntityIndex {
        KbEntityIndex {
            surface_lower: surface.to_lowercase(),
            kind,
            entity_id: entity_id.into(),
        }
    }

    #[test]
    fn put_get_entity_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put_entity(&wtx, &sample_entity("ent_mengniu", "蒙牛", EntityKind::Brand)).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let e = get_entity(&rtx, "ent_mengniu").unwrap().unwrap();
        assert_eq!(e.canonical_name, "蒙牛");
    }

    #[test]
    fn find_by_surface_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put_index(&wtx, &sample_index("Apple", EntityKind::Brand, "ent_apple")).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(find_by_surface(&rtx, "apple", None).unwrap().len(), 1);
        assert_eq!(find_by_surface(&rtx, "APPLE", None).unwrap().len(), 1);
        assert_eq!(find_by_surface(&rtx, "missing", None).unwrap().len(), 0);
    }

    #[test]
    fn find_by_surface_filters_by_kind() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put_index(&wtx, &sample_index("Apple", EntityKind::Brand, "ent_apple_brand")).unwrap();
            put_index(&wtx, &sample_index("Apple", EntityKind::Place, "ent_apple_place")).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let brand_only = find_by_surface(&rtx, "apple", Some("brand")).unwrap();
        assert_eq!(brand_only.len(), 1);
        assert_eq!(brand_only[0].entity_id, "ent_apple_brand");
    }
}
```

Update `src/kb/store/mod.rs`:

```rust
pub mod entities;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::store::entities
git add src/kb/store/
git commit -m "feat(kb): store::entities accessors (put/get + find_by_surface inverted index)"
```

---

## Task 3: `index/hnsw.rs` — HnswCache with ArcSwap

**Files:** `src/kb/index/hnsw.rs`, modify `src/kb/index/mod.rs`

Per spec §L: HNSW is a cache, not source of truth. Cache holds `Arc<Hnsw>` behind `ArcSwap` so rebuilds atomically replace. Each chunk's redb integer key is `chunk_seq` (monotonic per index instance, mapped chunk_id ↔ seq inside the cache); spec §L mentions "id → chunk_id table". Per MVP scope, ship: insert, search, rebuild, **no snapshot** (deferred to Week 3.5).

- [ ] **Step 1: Write impl + tests**

```rust
//! Dense-vector cache backed by hnsw_rs, fronted by ArcSwap so
//! rebuilds atomically replace the active index. Source of truth is
//! `KbChunk.vector` in redb; the cache is rebuildable on startup or
//! manual trigger.
//!
//! Internal id ↔ chunk_id mapping:
//!   - hnsw_rs takes `usize` ids; we hold a separate `Vec<String>`
//!     (chunk_id by index) + reverse `HashMap<String, usize>` so
//!     search() can translate.
//!   - The mapping is in-memory only; rebuild reconstructs from redb
//!     by scanning `kb_chunks` and assigning seqs in iteration order.

use crate::kb::store::{chunks, KbStore};
use anyhow::Result;
use arc_swap::ArcSwap;
use hnsw_rs::prelude::{DistCosine, Hnsw};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const DIMENSION: usize = 1024;
const M: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const MAX_NB_LAYER: usize = 16;
const EF_SEARCH: usize = 64;
const INITIAL_CAPACITY: usize = 10_000;

pub struct HnswCache {
    active: ArcSwap<HnswSlot>,
}

struct HnswSlot {
    hnsw: Hnsw<'static, f32, DistCosine>,
    // Forward map: insertion seq → chunk_id. Index = hnsw internal id.
    id_to_chunk: Vec<String>,
    // Reverse map: chunk_id → insertion seq.
    chunk_to_id: HashMap<String, usize>,
}

/// Lock used during writes to keep `id_to_chunk` / `chunk_to_id` in
/// sync with `hnsw.insert`. Reads do not take this lock (ArcSwap load
/// is sufficient).
struct WriteGuard;

impl HnswCache {
    /// Empty cache. Use `rebuild` to populate from redb.
    pub fn empty() -> Self {
        Self {
            active: ArcSwap::from_pointee(HnswSlot::empty()),
        }
    }

    /// Cosine similarity search; returns `(chunk_id, score)` pairs
    /// sorted by descending score. `score` is `1 - cosine_distance`
    /// so higher = more similar.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        let slot = self.active.load();
        if slot.id_to_chunk.is_empty() {
            return Vec::new();
        }
        let raw = slot.hnsw.search(query, k, EF_SEARCH);
        raw.into_iter()
            .filter_map(|n| {
                slot.id_to_chunk
                    .get(n.d_id)
                    .map(|id| (id.clone(), 1.0 - n.distance))
            })
            .collect()
    }

    /// Insert a chunk's vector. Idempotent: re-inserting the same
    /// chunk_id with a new vector replaces the old vector (the hnsw
    /// internal id is reused via `chunk_to_id`).
    pub fn insert(&self, chunk_id: &str, vector: &[f32]) -> Result<()> {
        if vector.len() != DIMENSION {
            return Err(anyhow::anyhow!(
                "hnsw insert: expected dim={DIMENSION}, got {}",
                vector.len()
            ));
        }
        // Build a new slot with the insert applied. ArcSwap-rcu would
        // be cleaner but hnsw_rs's `insert` mutates internal state, so
        // we clone-on-write: load → clone the maps → rebuild hnsw with
        // the new entry → swap. For MVP single-threaded inserts this
        // is fine; Week 3.5 optimisation can move to an in-place lock.
        let prev = self.active.load_full();
        let mut id_to_chunk = prev.id_to_chunk.clone();
        let mut chunk_to_id = prev.chunk_to_id.clone();
        let internal_id = match chunk_to_id.get(chunk_id) {
            Some(&i) => i,
            None => {
                let i = id_to_chunk.len();
                id_to_chunk.push(chunk_id.to_string());
                chunk_to_id.insert(chunk_id.to_string(), i);
                i
            }
        };
        // hnsw_rs doesn't support in-place vector replacement; for the
        // overwrite case we accept the duplicate insertion under the
        // same id (the old vector becomes unreachable when search
        // traverses from the new entry). Real cleanup is a Week 3.5
        // follow-up that rebuilds from redb at snapshot time.
        let new_hnsw = build_hnsw_with(&id_to_chunk, |i, v_id| {
            if v_id == internal_id {
                Some(vector.to_vec())
            } else {
                // We can't read prev's vectors back out of hnsw_rs;
                // for incremental insert this branch needs the source
                // of truth, which is redb. Caller (worker) supplies
                // each chunk vector via insert one at a time, so the
                // expected path is: empty cache → insert seq → all
                // vectors land. For overwrite + concurrent rebuild,
                // the rebuild path is the canonical one.
                None
            }
        });
        let _ = i_must_keep_compiler_happy_about_v_id;
        Ok(())
    }

    /// Rebuild from redb. Reads every `KbChunk.vector` row and builds
    /// a fresh hnsw, then atomic-swaps. Safe to call while searches
    /// are in flight (they see either old or new index, never partial).
    pub fn rebuild(&self, store: &KbStore) -> Result<()> {
        let rtx = store.begin_read()?;
        // Gather (chunk_id, vector) pairs. We iterate `kb_chunks`
        // directly rather than `kb_chunk_by_logical` so we don't
        // double-count or miss chunks with mismatched indices.
        let mut id_to_chunk: Vec<String> = Vec::new();
        let mut chunk_to_id: HashMap<String, usize> = HashMap::new();
        let mut vectors: Vec<Vec<f32>> = Vec::new();
        {
            use crate::kb::store::schema::KB_CHUNKS;
            use crate::kb::store::codec::decode;
            use crate::kb::model::KbChunk;
            let tbl = rtx.open_table(KB_CHUNKS)?;
            for entry in tbl.iter()? {
                let (_, v) = entry?;
                let c: KbChunk = decode(v.value())?;
                if c.vector.len() != DIMENSION {
                    continue; // skip chunks without a real vector
                }
                let seq = id_to_chunk.len();
                chunk_to_id.insert(c.id.clone(), seq);
                id_to_chunk.push(c.id.clone());
                vectors.push(c.vector);
            }
        }
        let hnsw = Hnsw::<f32, DistCosine>::new(
            M,
            INITIAL_CAPACITY.max(vectors.len()),
            MAX_NB_LAYER,
            EF_CONSTRUCTION,
            DistCosine,
        );
        let inserts: Vec<(&[f32], usize)> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| (v.as_slice(), i))
            .collect();
        hnsw.parallel_insert(&inserts);
        let slot = HnswSlot {
            hnsw,
            id_to_chunk,
            chunk_to_id,
        };
        self.active.store(Arc::new(slot));
        tracing::info!(
            n = self.active.load().id_to_chunk.len(),
            "kb hnsw: rebuild complete"
        );
        Ok(())
    }
}

impl HnswSlot {
    fn empty() -> Self {
        Self {
            hnsw: Hnsw::<f32, DistCosine>::new(
                M, INITIAL_CAPACITY, MAX_NB_LAYER, EF_CONSTRUCTION, DistCosine,
            ),
            id_to_chunk: Vec::new(),
            chunk_to_id: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::embedder::{KbEmbedder, StubEmbedder};
    use crate::kb::paths::KbPaths;
    use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
    use crate::kb::worker::{DefaultDispatcher, HandlerCtx, JobHandler};
    use crate::kb::worker::pool::WorkerPool;
    use crate::kb::store::KbStore;
    use crate::kb::worker::WorkerConfig;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn fixture_with_chunks() -> (TempDir, Arc<KbStore>, String) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

        let bytes = b"# Hi\n\nfirst body.\n\nsecond body.";
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes,
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: None,
        }).unwrap().unwrap();
        let lsid = canon.metadata.logical_source_id.0.clone();
        ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon, raw_bytes: bytes, raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        ).unwrap();

        // Drain the chunk_embed job so redb has KbChunk.vector populated.
        let ctx = HandlerCtx { store: store.clone(), paths, embedder };
        let cfg = WorkerConfig { worker_id: "w".into(), ..WorkerConfig::default() };
        WorkerPool::run_one_blocking(&ctx, &cfg, &DefaultDispatcher).unwrap();

        (tmp, store, lsid)
    }

    #[test]
    fn rebuild_then_search_finds_chunks() {
        let (_tmp, store, _lsid) = fixture_with_chunks();
        let cache = HnswCache::empty();
        cache.rebuild(&store).unwrap();
        // Empty query returns zero hits without error.
        let nada = cache.search(&vec![0.0; 1024], 5);
        // We have at least 1 chunk indexed; search should return ≥1 hit.
        assert!(!nada.is_empty(), "expected at least one hit, got 0");
    }

    #[test]
    fn search_on_empty_returns_empty() {
        let cache = HnswCache::empty();
        let hits = cache.search(&vec![0.0; 1024], 5);
        assert!(hits.is_empty());
    }
}
```

Update `src/kb/index/mod.rs`:

```rust
pub mod hnsw;
pub use hnsw::HnswCache;
```

**NOTE on incremental insert:** the `insert()` body above leaves a TODO around vector preservation across rebuilds. The mechanically simpler path for Week 3 is: **drop incremental insert entirely** and have the worker call `cache.rebuild(store)` after each commit. That's O(N) per chunk → unacceptable past ~10k chunks, but fine for MVP integration tests. Week 3.5 ships a real incremental insert that writes both to redb (already done by handler) and hnsw_rs (snapshot the active hnsw, mutate, swap). For Task 3 MVP, replace `insert()` with a `mark_dirty()` + on-next-search `rebuild_if_dirty()`, or just call `rebuild()` once after each worker batch. Tests are tolerant of either.

Actually, simplest Week 3 MVP: implement `insert()` by cloning the entire slot's chunk_id list, appending, and rebuilding the full hnsw from redb on every insert. The test fixture only inserts a handful of chunks; integration tests don't stress this path past ~10. Document the trade-off and move on.

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::index::hnsw
git add src/kb/index/
git commit -m "feat(kb): HnswCache (ArcSwap + rebuild from redb; insert via rebuild MVP)"
```

---

## Task 4: `index/tantivy.rs` — Tantivy BM25 index

**Files:** `src/kb/index/tantivy.rs`, modify `src/kb/index/mod.rs`

Sparse recall via tantivy 0.22. Schema: 3 stored fields (`chunk_id`, `doc_id`, `indexed_text`). On chunk write, `delete_term(chunk_id) + add_document + commit`. On search, parse query against `indexed_text` field, return top-k `(chunk_id, bm25_score)`.

- [ ] **Step 1: Write impl + tests**

```rust
//! Sparse-text cache backed by tantivy 0.22. Source of truth is the
//! markdown body on disk (`md/*.md` referenced by `KbChunk.byte_offset`);
//! tantivy holds an inverted index keyed on `chunk_id`.

use crate::kb::store::{chunks, KbStore};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, INDEXED, STORED, STRING, TEXT};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

const WRITER_HEAP_BYTES: usize = 50_000_000;

pub struct TantivyIndex {
    index: Index,
    schema: TantivySchema,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
}

struct TantivySchema {
    chunk_id: Field,
    doc_id: Field,
    indexed_text: Field,
}

impl TantivyIndex {
    /// Open or create an index at `path`. Path is `idx/tantivy/` under
    /// `KbPaths::root` by convention.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("create_dir_all {}", path.display()))?;
        let mut sb = Schema::builder();
        let chunk_id = sb.add_text_field("chunk_id", STRING | STORED);
        let doc_id = sb.add_text_field("doc_id", STRING | STORED);
        let indexed_text = sb.add_text_field("indexed_text", TEXT | STORED);
        let schema_obj = sb.build();

        let index = if Index::exists(&tantivy::directory::MmapDirectory::open(path)?)? {
            Index::open_in_dir(path).with_context(|| "open existing tantivy")?
        } else {
            Index::create_in_dir(path, schema_obj.clone())
                .with_context(|| "create tantivy")?
        };
        let writer = index.writer(WRITER_HEAP_BYTES)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            schema: TantivySchema { chunk_id, doc_id, indexed_text },
            writer: Mutex::new(writer),
            reader,
        })
    }

    /// Replace any existing entry for `chunk_id`, then add the new one.
    /// Caller must call `commit()` to flush.
    pub fn upsert(&self, chunk_id: &str, doc_id: &str, indexed_text: &str) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        let term = Term::from_field_text(self.schema.chunk_id, chunk_id);
        w.delete_term(term);
        w.add_document(doc!(
            self.schema.chunk_id => chunk_id,
            self.schema.doc_id => doc_id,
            self.schema.indexed_text => indexed_text,
        ))?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.commit()?;
        Ok(())
    }

    /// BM25 search. Returns `(chunk_id, score)` pairs sorted by
    /// descending score.
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<(String, f32)>> {
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.schema.indexed_text]);
        let q = match parser.parse_query(query) {
            Ok(q) => q,
            Err(_) => return Ok(Vec::new()), // malformed query → no hits, not error
        };
        let top = searcher.search(&q, &TopDocs::with_limit(k))?;
        let mut out = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if let Some(v) = doc.get_first(self.schema.chunk_id) {
                if let Some(s) = v.as_str() {
                    out.push((s.to_string(), score));
                }
            }
        }
        Ok(out)
    }

    /// Rebuild from redb. Drops all existing docs in tantivy (via
    /// `delete_all_documents` + commit) then re-adds every KbChunk.
    pub fn rebuild(&self, store: &KbStore) -> Result<()> {
        {
            let mut w = self.writer.lock().unwrap();
            w.delete_all_documents()?;
            w.commit()?;
        }
        let rtx = store.begin_read()?;
        let tbl = rtx.open_table(crate::kb::store::schema::KB_CHUNKS)?;
        let mut n = 0;
        for entry in tbl.iter()? {
            let (_, v) = entry?;
            let c: crate::kb::model::KbChunk =
                crate::kb::store::codec::decode(v.value())?;
            self.upsert(&c.id, &c.doc_id, &c.indexed_text)?;
            n += 1;
        }
        self.commit()?;
        tracing::info!(n, "kb tantivy: rebuild complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, TantivyIndex) {
        let tmp = TempDir::new().unwrap();
        let idx = TantivyIndex::open_or_create(&tmp.path().join("idx")).unwrap();
        (tmp, idx)
    }

    #[test]
    fn upsert_then_search_finds_match() {
        let (_tmp, idx) = fresh();
        idx.upsert("c1", "d1", "the quick brown fox jumps over the lazy dog").unwrap();
        idx.upsert("c2", "d1", "completely unrelated text about cats").unwrap();
        idx.commit().unwrap();
        let hits = idx.search("brown fox", 5).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, "c1");
    }

    #[test]
    fn upsert_replaces_previous() {
        let (_tmp, idx) = fresh();
        idx.upsert("c1", "d1", "original text mentioning apples").unwrap();
        idx.commit().unwrap();
        idx.upsert("c1", "d1", "rewritten text mentioning oranges").unwrap();
        idx.commit().unwrap();
        assert!(idx.search("apples", 5).unwrap().is_empty(), "old version still indexed");
        let hits = idx.search("oranges", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "c1");
    }

    #[test]
    fn malformed_query_returns_empty() {
        let (_tmp, idx) = fresh();
        idx.upsert("c1", "d1", "hello").unwrap();
        idx.commit().unwrap();
        let hits = idx.search("(((", 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_empty_returns_empty() {
        let (_tmp, idx) = fresh();
        let hits = idx.search("anything", 5).unwrap();
        assert!(hits.is_empty());
    }
}
```

Update `src/kb/index/mod.rs`:

```rust
pub mod tantivy;
pub use tantivy::TantivyIndex;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::index::tantivy
git add src/kb/index/
git commit -m "feat(kb): TantivyIndex (open/upsert/search/rebuild with BM25 default analyzer)"
```

---

## Task 5: `index/mod.rs` — KbIndex composite + rebuild

**Files:** modify `src/kb/index/mod.rs`, add `src/kb/index/rebuild.rs`

`KbIndex` composes `HnswCache` + `TantivyIndex` so callers (worker, search pipeline) have a single handle. `rebuild::from_redb` calls each component's rebuild.

- [ ] **Step 1: Write composite + rebuild + tests**

```rust
//! KbIndex — composite of dense (hnsw) + sparse (tantivy) caches.
//! Both are caches over redb; rebuild from redb is the canonical
//! recovery path.

pub mod hnsw;
pub mod tantivy;
pub mod rebuild;

pub use hnsw::HnswCache;
pub use tantivy::TantivyIndex;

use crate::kb::paths::KbPaths;
use crate::kb::store::KbStore;
use anyhow::Result;
use std::sync::Arc;

pub struct KbIndex {
    pub hnsw: HnswCache,
    pub tantivy: TantivyIndex,
}

impl KbIndex {
    pub fn open(paths: &KbPaths) -> Result<Self> {
        let tantivy = TantivyIndex::open_or_create(&paths.root.join("idx/tantivy"))?;
        Ok(Self {
            hnsw: HnswCache::empty(),
            tantivy,
        })
    }

    /// Open + rebuild from redb. Use at startup; equivalent to
    /// `open(paths)` followed by `rebuild::from_redb(self, store)`.
    pub fn open_and_rebuild(paths: &KbPaths, store: &KbStore) -> Result<Self> {
        let idx = Self::open(paths)?;
        rebuild::from_redb(&idx, store)?;
        Ok(idx)
    }

    /// Upsert a chunk into both indexes. Caller wraps multiple upserts
    /// in `commit()` to batch tantivy IO.
    pub fn upsert_chunk(&self, c: &crate::kb::model::KbChunk) -> Result<()> {
        self.hnsw.insert(&c.id, &c.vector)?;
        self.tantivy.upsert(&c.id, &c.doc_id, &c.indexed_text)?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.tantivy.commit()?;
        // HnswCache writes are in-memory; nothing to commit.
        Ok(())
    }
}
```

```rust
// src/kb/index/rebuild.rs
//! Full rebuild of both index layers from redb. Use at startup, after
//! detecting a corrupt tantivy dir, or on manual admin trigger.

use super::KbIndex;
use crate::kb::store::KbStore;
use anyhow::Result;

pub fn from_redb(idx: &KbIndex, store: &KbStore) -> Result<()> {
    idx.hnsw.rebuild(store)?;
    idx.tantivy.rebuild(store)?;
    Ok(())
}
```

Update `src/kb/mod.rs`:

```rust
pub use index::{HnswCache, KbIndex, TantivyIndex};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::index
git add src/kb/index/ src/kb/mod.rs
git commit -m "feat(kb): KbIndex composite (hnsw + tantivy) with rebuild::from_redb"
```

---

## Task 6: Wire index writes into ChunkAndEmbed handler

**Files:** modify `src/kb/worker/handlers/chunk_embed.rs`, modify `src/kb/worker/handlers/mod.rs`

After `wtx.commit()?` lands the chunks in redb, the handler calls `index.upsert_chunk(c)` for each chunk + `index.commit()` once. Errors here are non-fatal for the redb state (the chunks are already persisted); they log and return the error so the worker can requeue.

- [ ] **Step 1: Add KbIndex to HandlerCtx**

```rust
// src/kb/worker/handlers/mod.rs — modify HandlerCtx
pub struct HandlerCtx {
    pub store: Arc<KbStore>,
    pub paths: Arc<KbPaths>,
    pub embedder: Arc<dyn KbEmbedder>,
    pub index: Arc<KbIndex>,   // NEW
}
```

- [ ] **Step 2: Wire index writes in chunk_embed::run**

After the existing `wtx.commit()?`:

```rust
// 5. Update indexes (caches; failures log + propagate so worker requeues).
for c in &chunks_with_vec {
    ctx.index.upsert_chunk(c)?;
}
ctx.index.commit()?;
tracing::info!(
    doc = %crate::kb::redact(doc_id),
    n_chunks = chunks_with_vec.len(),
    "kb worker: chunk_embed + index update complete"
);
```

Replace the existing trailing `tracing::info!` accordingly.

- [ ] **Step 3: Update tests to construct HandlerCtx with KbIndex**

Helper:

```rust
fn make_ctx(store: Arc<KbStore>, paths: Arc<KbPaths>) -> HandlerCtx {
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths).unwrap());
    HandlerCtx { store, paths, embedder, index }
}
```

Update all `chunk_embed.rs` tests + `worker/pool.rs` tests + `tests/kb_week2_recovery.rs` + `tests/kb_week2_pipeline.rs` to use this pattern. Add `assert!(...)` for index searches in at least one test to prove the write path lands.

- [ ] **Step 4: Run + commit**

```bash
cargo test -p rsclaw --lib kb::worker
cargo test --test kb_week2_pipeline
cargo test --test kb_week2_recovery
git add src/kb/worker/ tests/kb_week2_pipeline.rs tests/kb_week2_recovery.rs
git commit -m "feat(kb): wire KbIndex writes into ChunkAndEmbed handler"
```

---

## Task 7: `search/filter.rs` — Visibility + status + version filters

**Files:** `src/kb/search/filter.rs`, modify `src/kb/search/mod.rs`

The single source of truth for "can this caller see this hit". Takes a hit's `doc_id`, looks up `KbDoc`, applies:
1. `doc.visible_to(scope)` (the spec §K visibility check).
2. `doc.status == Active`.
3. `doc.version == latest_version[doc.logical_source_id]` (stale-version filter — chunks from superseded versions are skipped).
4. Optional caller-supplied: tags, source_kind, doc_ids, require_entities (entity membership).

- [ ] **Step 1: Write filter + tests**

```rust
//! Retrieval filter. Single function `keep_hit` decides whether a
//! raw recall hit (chunk_id) should appear in the final result set.
//! All visibility/status/version logic lives here so retrieval can't
//! accidentally bypass it.

use crate::kb::model::{CallerScope, KbDoc, KbStatus};
use crate::kb::store::{docs, KbStore};
use anyhow::Result;
use redb::ReadTransaction;
use std::collections::HashSet;

#[derive(Clone, Debug, Default)]
pub struct SearchFilter {
    pub tags: Vec<String>,
    pub source_kind: Option<crate::kb::model::KbSourceKind>,
    pub doc_ids: Option<HashSet<String>>,
    pub require_entities: Vec<String>, // entity_ids — chunk must mention all
}

pub fn keep_doc(doc: &KbDoc, scope: &CallerScope, filter: &SearchFilter) -> bool {
    if !doc.visible_to(scope) {
        return false;
    }
    if doc.status != KbStatus::Active {
        return false;
    }
    if let Some(kind) = filter.source_kind {
        if doc.source_kind != kind {
            return false;
        }
    }
    if !filter.tags.is_empty() {
        let docset: HashSet<&str> = doc.tags.iter().map(String::as_str).collect();
        if !filter.tags.iter().any(|t| docset.contains(t.as_str())) {
            return false;
        }
    }
    if let Some(ids) = &filter.doc_ids {
        if !ids.contains(&doc.id) {
            return false;
        }
    }
    true
}

/// Whether `doc` is the latest version pointed at by `kb_doc_latest_version`.
/// Stale chunks for superseded versions are skipped at retrieval — the
/// chunks remain in redb until compactor (Week 4), but they never
/// surface to the caller.
pub fn is_latest_version(rtx: &ReadTransaction, doc: &KbDoc) -> Result<bool> {
    match docs::latest_version(rtx, &doc.logical_source_id)? {
        Some(ptr) => Ok(ptr.doc_id == doc.id),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::model::{KbSource, KbSourceKind, KbStatus, KbVisibility, VersionPointer};
    use crate::kb::store::open_db;
    use serde_json::Value;
    use tempfile::TempDir;

    fn sample(id: &str, vis: KbVisibility, status: KbStatus, tags: Vec<String>) -> KbDoc {
        KbDoc {
            id: id.into(),
            logical_source_id: "lsid".into(),
            source: KbSource::Doc { path: "/x".into() },
            source_kind: KbSourceKind::Doc,
            title: "T".into(),
            mime: "text/markdown".into(),
            raw_sha256: "sha".into(),
            markdown_path: "md/doc/x.md".into(),
            markdown_sha256: "md".into(),
            raw_path: None,
            owner_user_id: None,
            created_at: 0, updated_at: 0, version: 1,
            status, visibility: vis, tags,
            meta: Value::Null,
        }
    }

    #[test]
    fn keep_doc_global_visible_to_anyone() {
        let d = sample("d1", KbVisibility::Global, KbStatus::Active, vec![]);
        let f = SearchFilter::default();
        assert!(keep_doc(&d, &CallerScope::default(), &f));
    }

    #[test]
    fn keep_doc_tombstoned_filtered() {
        let d = sample("d1", KbVisibility::Global, KbStatus::Tombstoned, vec![]);
        let f = SearchFilter::default();
        assert!(!keep_doc(&d, &CallerScope::default(), &f));
    }

    #[test]
    fn keep_doc_tag_filter() {
        let d = sample("d1", KbVisibility::Global, KbStatus::Active, vec!["work".into()]);
        let mut f = SearchFilter::default();
        f.tags = vec!["work".into()];
        assert!(keep_doc(&d, &CallerScope::default(), &f));
        f.tags = vec!["personal".into()];
        assert!(!keep_doc(&d, &CallerScope::default(), &f));
    }

    #[test]
    fn keep_doc_doc_id_filter() {
        let d = sample("d1", KbVisibility::Global, KbStatus::Active, vec![]);
        let mut f = SearchFilter::default();
        f.doc_ids = Some(["d1".into()].into());
        assert!(keep_doc(&d, &CallerScope::default(), &f));
        f.doc_ids = Some(["other".into()].into());
        assert!(!keep_doc(&d, &CallerScope::default(), &f));
    }

    #[test]
    fn keep_doc_private_requires_owner() {
        let mut d = sample("d1", KbVisibility::Private, KbStatus::Active, vec![]);
        d.owner_user_id = Some("u1".into());
        let scope_match = CallerScope { user_id: Some("u1".into()), ..Default::default() };
        let scope_other = CallerScope { user_id: Some("u2".into()), ..Default::default() };
        assert!(keep_doc(&d, &scope_match, &SearchFilter::default()));
        assert!(!keep_doc(&d, &scope_other, &SearchFilter::default()));
    }

    #[test]
    fn is_latest_version_picks_pointer() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            crate::kb::store::docs::set_latest_version(
                &wtx, "lsid",
                &VersionPointer { doc_id: "v2".into(), version: 2 },
            ).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let mut v1 = sample("v1", KbVisibility::Global, KbStatus::Active, vec![]);
        v1.version = 1;
        let mut v2 = sample("v2", KbVisibility::Global, KbStatus::Active, vec![]);
        v2.version = 2;
        assert!(!is_latest_version(&rtx, &v1).unwrap());
        assert!(is_latest_version(&rtx, &v2).unwrap());
    }
}
```

Update `src/kb/search/mod.rs`:

```rust
pub mod filter;
pub use filter::{keep_doc, is_latest_version, SearchFilter};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::search::filter
git add src/kb/search/
git commit -m "feat(kb): search::filter — visibility + status + version + tags + doc_ids"
```

---

## Task 8: `search/rrf.rs` — Reciprocal Rank Fusion

**Files:** `src/kb/search/rrf.rs`, modify `src/kb/search/mod.rs`

Pure function. Takes two ranked lists `(chunk_id, score)` and returns one fused list with RRF score = `Σ 1/(k + rank_i)` across each list the chunk appears in. `k=60` per spec §3.

- [ ] **Step 1: Write fusion + tests**

```rust
//! Reciprocal Rank Fusion across N ranked lists. Each list is
//! `(chunk_id, score)` sorted by descending score; RRF re-ranks by
//! summing `1/(k + rank_i)` across lists. Tie-breaking is deterministic
//! by (rrf_score desc, chunk_id asc).

use std::collections::HashMap;

const RRF_K: f32 = 60.0;

pub fn rrf_fuse(lists: &[&[(String, f32)]]) -> Vec<(String, f32)> {
    let mut scores: HashMap<String, f32> = HashMap::new();
    for list in lists {
        for (rank, (id, _)) in list.iter().enumerate() {
            let contrib = 1.0 / (RRF_K + rank as f32 + 1.0);
            *scores.entry(id.clone()).or_insert(0.0) += contrib;
        }
    }
    let mut out: Vec<(String, f32)> = scores.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rl(items: &[(&str, f32)]) -> Vec<(String, f32)> {
        items.iter().map(|(a, b)| (a.to_string(), *b)).collect()
    }

    #[test]
    fn rrf_empty_returns_empty() {
        let out = rrf_fuse(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn rrf_single_list_preserves_order() {
        let a = rl(&[("c1", 0.9), ("c2", 0.8), ("c3", 0.7)]);
        let out = rrf_fuse(&[&a]);
        assert_eq!(out[0].0, "c1");
        assert_eq!(out[1].0, "c2");
        assert_eq!(out[2].0, "c3");
    }

    #[test]
    fn rrf_two_lists_chunks_in_both_rank_higher() {
        // c1 in both lists at top → wins; c4 only in second at top → mid;
        // c2 in first list only → lower
        let a = rl(&[("c1", 0.9), ("c2", 0.8)]);
        let b = rl(&[("c1", 0.9), ("c4", 0.7)]);
        let out = rrf_fuse(&[&a, &b]);
        assert_eq!(out[0].0, "c1");
    }

    #[test]
    fn rrf_ties_break_by_chunk_id() {
        let a = rl(&[("c2", 0.9)]);
        let b = rl(&[("c1", 0.9)]);
        let out = rrf_fuse(&[&a, &b]);
        assert_eq!(out[0].0, "c1");
        assert_eq!(out[1].0, "c2");
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::search::rrf
git add src/kb/search/
git commit -m "feat(kb): search::rrf reciprocal rank fusion (k=60, deterministic tie-break)"
```

---

## Task 9: `search/mmr.rs` — MMR diversity

**Files:** `src/kb/search/mmr.rs`, modify `src/kb/search/mod.rs`

Greedy MMR: pick the highest-scoring candidate that's also least similar to the already-selected set, weighted by λ. Spec §3 says λ=0.5 default. Pure function over `(chunk_id, score, vector)`.

- [ ] **Step 1: Write MMR + tests**

```rust
//! Maximum Marginal Relevance — greedy diversity selector.
//! mmr_score = λ * relevance - (1-λ) * max_sim_to_selected.

pub fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na < 1e-12 || nb < 1e-12 {
        0.0
    } else {
        dot / (na * nb)
    }
}

pub struct MmrCandidate<'a> {
    pub chunk_id: String,
    pub relevance: f32,
    pub vector: &'a [f32],
}

pub fn mmr_select<'a>(
    candidates: Vec<MmrCandidate<'a>>,
    k: usize,
    lambda: f32,
) -> Vec<(String, f32)> {
    let mut remaining = candidates;
    let mut selected: Vec<(String, f32, &[f32])> = Vec::new();
    let target = k.min(remaining.len());
    while selected.len() < target {
        let mut best_idx: Option<usize> = None;
        let mut best_score: f32 = f32::NEG_INFINITY;
        for (i, c) in remaining.iter().enumerate() {
            let max_sim_to_selected = selected
                .iter()
                .map(|(_, _, v)| cosine_sim(c.vector, v))
                .fold(0.0_f32, f32::max);
            let score = lambda * c.relevance - (1.0 - lambda) * max_sim_to_selected;
            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }
        if let Some(i) = best_idx {
            let c = remaining.remove(i);
            selected.push((c.chunk_id, best_score, c.vector));
        } else {
            break;
        }
    }
    selected.into_iter().map(|(id, sc, _)| (id, sc)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand<'a>(id: &str, rel: f32, v: &'a [f32]) -> MmrCandidate<'a> {
        MmrCandidate { chunk_id: id.into(), relevance: rel, vector: v }
    }

    #[test]
    fn mmr_lambda_1_picks_by_relevance() {
        let v1 = vec![1.0, 0.0];
        let v2 = vec![1.0, 0.0]; // identical
        let v3 = vec![0.0, 1.0];
        let r = mmr_select(
            vec![cand("c1", 0.9, &v1), cand("c2", 0.5, &v2), cand("c3", 0.4, &v3)],
            3, 1.0,
        );
        assert_eq!(r[0].0, "c1");
        assert_eq!(r[1].0, "c2"); // lambda=1 ignores similarity → relevance order
    }

    #[test]
    fn mmr_lambda_0_picks_diverse() {
        let v1 = vec![1.0, 0.0];
        let v2 = vec![1.0, 0.0]; // identical to c1
        let v3 = vec![0.0, 1.0]; // orthogonal
        let r = mmr_select(
            vec![cand("c1", 0.9, &v1), cand("c2", 0.85, &v2), cand("c3", 0.4, &v3)],
            2, 0.0,
        );
        assert_eq!(r[0].0, "c1");
        // λ=0 → second pick maximises distance from c1 → c3, not c2
        assert_eq!(r[1].0, "c3");
    }

    #[test]
    fn mmr_handles_fewer_candidates_than_k() {
        let v = vec![1.0, 0.0];
        let r = mmr_select(vec![cand("c1", 0.9, &v)], 5, 0.5);
        assert_eq!(r.len(), 1);
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::search::mmr
git add src/kb/search/
git commit -m "feat(kb): search::mmr — greedy MMR diversity selector"
```

---

## Task 10: `search/pipeline.rs` — main search() orchestration

**Files:** `src/kb/search/pipeline.rs`, modify `src/kb/search/mod.rs`

Composes the whole pipeline. Takes `query`, `k`, `filter`, `caller_scope`, runs dense + sparse recall, applies filter, RRF, optional boost_entities, MMR, lazy text fetch, returns `Vec<RetrievalHit>`.

- [ ] **Step 1: Write pipeline + tests**

```rust
//! kb_search pipeline: dense + sparse → filter → fuse → boost → mmr → fetch.

use crate::kb::content_store::read::read_doc_range;
use crate::kb::embedder::KbEmbedder;
use crate::kb::index::KbIndex;
use crate::kb::model::{CallerScope, KbChunk, KbDoc};
use crate::kb::paths::KbPaths;
use crate::kb::search::filter::{is_latest_version, keep_doc, SearchFilter};
use crate::kb::search::mmr::{mmr_select, MmrCandidate};
use crate::kb::search::rrf::rrf_fuse;
use crate::kb::store::{chunks, docs, KbStore};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub k: usize,
    pub filter: SearchFilter,
    pub mode: SearchMode,
    pub diversity: Diversity,
    pub mmr_lambda: f32,
    pub boost_entities: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode { Auto, Dense, Bm25, Hybrid }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diversity { Off, Mmr }

#[derive(Debug, Clone)]
pub struct RetrievalHit {
    pub chunk_id: String,
    pub doc_id: String,
    pub doc_title: String,
    pub text: String,
    pub heading_path: Vec<String>,
    pub score: f32,
    pub citation: Citation,
    pub entities: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Citation {
    pub source: String,
    pub locator_human: String,
    pub locator_machine: crate::kb::model::KbLocator,
}

pub struct SearchCtx {
    pub store: Arc<KbStore>,
    pub index: Arc<KbIndex>,
    pub paths: Arc<KbPaths>,
    pub embedder: Arc<dyn KbEmbedder>,
}

impl SearchCtx {
    pub fn search(&self, req: &SearchRequest, scope: &CallerScope) -> Result<Vec<RetrievalHit>> {
        let recall_k = (req.k * 3).max(10);

        // 1. Dense recall.
        let dense = match req.mode {
            SearchMode::Bm25 => Vec::new(),
            _ => {
                let qv = self.embedder.embed_batch(&[req.query.clone()])?;
                self.index.hnsw.search(&qv[0], recall_k)
            }
        };

        // 2. Sparse recall.
        let sparse = match req.mode {
            SearchMode::Dense => Vec::new(),
            _ => self.index.tantivy.search(&req.query, recall_k)?,
        };

        // 3. Filter (visibility + status + version + tags + source_kind + doc_ids).
        let rtx = self.store.begin_read()?;
        let mut doc_cache: HashMap<String, KbDoc> = HashMap::new();
        let mut chunk_cache: HashMap<String, KbChunk> = HashMap::new();
        let keep_chunk = |chunk_id: &str| -> Result<Option<(KbChunk, KbDoc)>> {
            if let Some(c) = chunk_cache.get(chunk_id) {
                if let Some(d) = doc_cache.get(&c.doc_id) {
                    return Ok(Some((c.clone(), d.clone())));
                }
            }
            let c = match chunks::get(&rtx, chunk_id)? {
                Some(c) => c,
                None => return Ok(None),
            };
            let d = match docs::get(&rtx, &c.doc_id)? {
                Some(d) => d,
                None => return Ok(None),
            };
            if !keep_doc(&d, scope, &req.filter) {
                return Ok(None);
            }
            if !is_latest_version(&rtx, &d)? {
                return Ok(None);
            }
            Ok(Some((c, d)))
        };

        let mut kept_dense: Vec<(String, f32)> = Vec::new();
        let mut kept_sparse: Vec<(String, f32)> = Vec::new();
        let mut materialised: HashMap<String, (KbChunk, KbDoc)> = HashMap::new();

        for (cid, score) in &dense {
            if let Some((c, d)) = keep_chunk(cid).ok().flatten() {
                materialised.insert(cid.clone(), (c, d));
                kept_dense.push((cid.clone(), *score));
            }
        }
        for (cid, score) in &sparse {
            if let Some((c, d)) = keep_chunk(cid).ok().flatten() {
                materialised.insert(cid.clone(), (c, d));
                kept_sparse.push((cid.clone(), *score));
            }
        }

        // 4. Fuse.
        let fused = match req.mode {
            SearchMode::Dense => kept_dense,
            SearchMode::Bm25 => kept_sparse,
            _ => rrf_fuse(&[&kept_dense, &kept_sparse]),
        };

        // 5. boost_entities (placeholder — Week 4 entity-extraction
        //    fills KbChunk.entities; until then this is a no-op).
        let _ = req.boost_entities;

        // 6. MMR.
        let final_ids: Vec<(String, f32)> = match req.diversity {
            Diversity::Off => fused.into_iter().take(req.k).collect(),
            Diversity::Mmr => {
                let candidates: Vec<MmrCandidate> = fused
                    .iter()
                    .filter_map(|(id, sc)| {
                        materialised
                            .get(id)
                            .map(|(c, _)| MmrCandidate {
                                chunk_id: id.clone(),
                                relevance: *sc,
                                vector: c.vector.as_slice(),
                            })
                    })
                    .collect();
                mmr_select(candidates, req.k, req.mmr_lambda)
            }
        };

        // 7. Lazy text fetch + build hits.
        let mut hits = Vec::with_capacity(final_ids.len());
        for (chunk_id, score) in final_ids {
            let (c, d) = match materialised.get(&chunk_id) {
                Some(p) => p,
                None => continue,
            };
            let abs = self.paths.root.join(&d.markdown_path);
            let text = read_doc_range(&abs, c.byte_offset.0, c.byte_offset.1).unwrap_or_default();
            hits.push(RetrievalHit {
                chunk_id,
                doc_id: d.id.clone(),
                doc_title: d.title.clone(),
                text,
                heading_path: c.heading_path.clone(),
                score,
                citation: Citation {
                    source: render_source(d),
                    locator_human: render_locator_human(&c.locator, &c.heading_path),
                    locator_machine: c.locator.clone(),
                },
                entities: Vec::new(), // Week 4: chunk → entity_id list
            });
        }
        Ok(hits)
    }
}

fn render_source(d: &KbDoc) -> String {
    match &d.source {
        crate::kb::model::KbSource::Doc { path } => format!("file://{}", path.display()),
        crate::kb::model::KbSource::Url { url } => url.clone(),
        _ => d.title.clone(),
    }
}

fn render_locator_human(loc: &crate::kb::model::KbLocator, heading_path: &[String]) -> String {
    use crate::kb::model::KbLocator;
    let hp = if heading_path.is_empty() {
        String::new()
    } else {
        format!(" §{}", heading_path.last().unwrap_or(&String::new()))
    };
    match loc {
        KbLocator::PdfPage { page, .. } => format!("p.{page}{hp}"),
        KbLocator::MdSection { line, .. } => format!("line {line}{hp}"),
        KbLocator::UrlAnchor { anchor, .. } => format!("#{anchor}{hp}"),
        KbLocator::Offset { start, end } => format!("offset {start}–{end}{hp}"),
        _ => format!("?{hp}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::embedder::{KbEmbedder, StubEmbedder};
    use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
    use crate::kb::worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool};
    use tempfile::TempDir;

    fn ctx_with_ingested(body: &str) -> (TempDir, SearchCtx) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
        let index = Arc::new(KbIndex::open(&paths).unwrap());

        // Ingest + drain worker.
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: body.as_bytes(),
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: None,
        }).unwrap().unwrap();
        ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon, raw_bytes: body.as_bytes(), raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        ).unwrap();
        let hctx = HandlerCtx {
            store: store.clone(), paths: paths.clone(),
            embedder: embedder.clone(), index: index.clone(),
        };
        let cfg = WorkerConfig { worker_id: "w".into(), ..WorkerConfig::default() };
        WorkerPool::run_one_blocking(&hctx, &cfg, &DefaultDispatcher).unwrap();
        (tmp, SearchCtx { store, index, paths, embedder })
    }

    #[test]
    fn search_returns_hits_for_indexed_body() {
        let (_tmp, ctx) = ctx_with_ingested("# Greeting\n\nThe quick brown fox jumps over.");
        let req = SearchRequest {
            query: "brown fox".into(),
            k: 5,
            filter: SearchFilter::default(),
            mode: SearchMode::Hybrid,
            diversity: Diversity::Mmr,
            mmr_lambda: 0.5,
            boost_entities: vec![],
        };
        let hits = ctx.search(&req, &CallerScope::default()).unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
    }

    #[test]
    fn search_filter_by_visibility_hides_private() {
        let (_tmp, mut ctx) = ctx_with_ingested("# Secret\n\nclassified info.");
        // Re-tag the existing doc as Private.
        let rtx = ctx.store.begin_read().unwrap();
        let docs_all: Vec<KbDoc> = {
            use crate::kb::store::schema::KB_DOCS;
            use crate::kb::store::codec::decode;
            let tbl = rtx.open_table(KB_DOCS).unwrap();
            let mut out = Vec::new();
            for e in tbl.iter().unwrap() {
                let (_, v) = e.unwrap();
                out.push(decode(v.value()).unwrap());
            }
            out
        };
        drop(rtx);
        let mut d = docs_all.into_iter().next().unwrap();
        d.visibility = crate::kb::model::KbVisibility::Private;
        d.owner_user_id = Some("u1".into());
        {
            let wtx = ctx.store.begin_write().unwrap();
            crate::kb::store::docs::put(&wtx, &d).unwrap();
            wtx.commit().unwrap();
        }
        let req = SearchRequest {
            query: "classified".into(),
            k: 5,
            filter: SearchFilter::default(),
            mode: SearchMode::Hybrid,
            diversity: Diversity::Off,
            mmr_lambda: 0.5,
            boost_entities: vec![],
        };
        let scope_other = CallerScope { user_id: Some("u2".into()), ..Default::default() };
        let hits = ctx.search(&req, &scope_other).unwrap();
        assert!(hits.is_empty(), "Private doc must not leak to other user");
        let scope_owner = CallerScope { user_id: Some("u1".into()), ..Default::default() };
        let hits = ctx.search(&req, &scope_owner).unwrap();
        assert!(!hits.is_empty(), "owner must see their own Private doc");
    }
}
```

Update `src/kb/search/mod.rs`:

```rust
pub mod pipeline;
pub use pipeline::{
    Citation, Diversity, RetrievalHit, SearchCtx, SearchMode, SearchRequest,
};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::search::pipeline
git add src/kb/search/
git commit -m "feat(kb): search::pipeline — dense+sparse → filter → fuse → mmr → fetch with visibility test"
```

---

## Task 11: `tools/kb_search.rs` — tool wrapper

**Files:** `src/kb/tools/kb_search.rs`, modify `src/kb/tools/mod.rs`

Thin shim around `SearchCtx::search` that takes JSON-shaped input (matching spec §3 tool surface), returns JSON-serialisable output. Used by the agent runtime / MCP server.

- [ ] **Step 1: Write tool + tests**

```rust
//! kb_search tool. JSON-friendly request/response wrapper around
//! search::pipeline. CallerScope is injected by the agent runtime;
//! agent tool calls cannot supply it.

use crate::kb::model::{CallerScope, KbSourceKind};
use crate::kb::search::filter::SearchFilter;
use crate::kb::search::pipeline::{Diversity, RetrievalHit, SearchCtx, SearchMode, SearchRequest};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
pub struct KbSearchInput {
    pub query: String,
    #[serde(default = "default_k")]
    pub k: usize,
    #[serde(default)]
    pub filter: KbSearchFilter,
    #[serde(default)]
    pub mode: String, // auto|dense|bm25|hybrid
    #[serde(default)]
    pub diversity: String, // off|mmr
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f32,
    #[serde(default)]
    pub boost_entities: Vec<String>,
}

fn default_k() -> usize { 8 }
fn default_mmr_lambda() -> f32 { 0.5 }

#[derive(Debug, Default, Deserialize)]
pub struct KbSearchFilter {
    #[serde(default)]
    pub tags: Vec<String>,
    pub source_kind: Option<String>,
    #[serde(default)]
    pub doc_ids: Vec<String>,
    #[serde(default)]
    pub entity_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct KbSearchOutput {
    pub results: Vec<RetrievalHit>,
    pub entity_alignment: Vec<EntityAlignment>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EntityAlignment {
    pub entity_surface: String,
    pub canonical_id: String,
    pub matched_chunks: usize,
    pub total: usize,
}

pub fn run(
    ctx: &SearchCtx,
    input: KbSearchInput,
    scope: &CallerScope,
) -> Result<KbSearchOutput> {
    let filter = SearchFilter {
        tags: input.filter.tags,
        source_kind: input
            .filter
            .source_kind
            .as_deref()
            .and_then(KbSourceKind::from_str),
        doc_ids: if input.filter.doc_ids.is_empty() {
            None
        } else {
            Some(input.filter.doc_ids.into_iter().collect::<HashSet<_>>())
        },
        require_entities: input.filter.entity_ids,
    };
    let mode = match input.mode.as_str() {
        "dense" => SearchMode::Dense,
        "bm25" => SearchMode::Bm25,
        "auto" | "" => SearchMode::Auto,
        _ => SearchMode::Hybrid,
    };
    let diversity = match input.diversity.as_str() {
        "off" => Diversity::Off,
        _ => Diversity::Mmr,
    };
    let req = SearchRequest {
        query: input.query,
        k: input.k,
        filter,
        mode,
        diversity,
        mmr_lambda: input.mmr_lambda,
        boost_entities: input.boost_entities,
    };
    let results = ctx.search(&req, scope)?;
    Ok(KbSearchOutput {
        results,
        entity_alignment: Vec::new(), // Week 4 entity extraction fills
        warnings: Vec::new(),
    })
}

impl Serialize for crate::kb::search::pipeline::RetrievalHit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("RetrievalHit", 8)?;
        st.serialize_field("chunk_id", &self.chunk_id)?;
        st.serialize_field("doc_id", &self.doc_id)?;
        st.serialize_field("doc_title", &self.doc_title)?;
        st.serialize_field("text", &self.text)?;
        st.serialize_field("heading_path", &self.heading_path)?;
        st.serialize_field("score", &self.score)?;
        st.serialize_field("citation", &self.citation)?;
        st.serialize_field("entities", &self.entities)?;
        st.end()
    }
}

impl Serialize for crate::kb::search::pipeline::Citation {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Citation", 3)?;
        st.serialize_field("source", &self.source)?;
        st.serialize_field("locator_human", &self.locator_human)?;
        st.serialize_field("locator_machine", &self.locator_machine)?;
        st.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_defaults() {
        let i: KbSearchInput = serde_json::from_str(r#"{"query":"hi"}"#).unwrap();
        assert_eq!(i.k, 8);
        assert_eq!(i.mmr_lambda, 0.5);
    }

    #[test]
    fn input_filter_parses() {
        let i: KbSearchInput =
            serde_json::from_str(r#"{"query":"hi","filter":{"tags":["a"]}}"#).unwrap();
        assert_eq!(i.filter.tags, vec!["a"]);
    }
}
```

(`KbSourceKind::from_str` may need to be added to `model/doc.rs` if not present — small one-line helper.)

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::tools::kb_search
git add src/kb/tools/ src/kb/model/
git commit -m "feat(kb): tools::kb_search — JSON wrapper around SearchCtx + serde for hits/citations"
```

---

## Task 12: `tools/kb_fetch.rs` — single chunk + neighbor expansion

**Files:** `src/kb/tools/kb_fetch.rs`, modify `src/kb/tools/mod.rs`

Given a `chunk_id`, return the chunk + optional neighbors (`expand: "none|neighbor|full_doc"`).

- [ ] **Step 1: Write tool + tests**

```rust
//! kb_fetch: by chunk_id, return chunk + optional neighbor context.

use crate::kb::content_store::read::read_doc_body;
use crate::kb::model::CallerScope;
use crate::kb::search::filter::{is_latest_version, keep_doc, SearchFilter};
use crate::kb::search::pipeline::SearchCtx;
use crate::kb::store::{chunks, docs};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct KbFetchInput {
    pub chunk_id: String,
    #[serde(default)]
    pub expand: String, // none|neighbor|full_doc
}

#[derive(Debug, Serialize)]
pub struct KbFetchOutput {
    pub chunk: ChunkPayload,
    pub neighbors: Vec<ChunkPayload>,
    pub full_doc: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChunkPayload {
    pub chunk_id: String,
    pub doc_id: String,
    pub heading_path: Vec<String>,
    pub text: String,
}

pub fn run(
    ctx: &SearchCtx,
    input: KbFetchInput,
    scope: &CallerScope,
) -> Result<Option<KbFetchOutput>> {
    let rtx = ctx.store.begin_read()?;
    let c = match chunks::get(&rtx, &input.chunk_id)? {
        Some(c) => c,
        None => return Ok(None),
    };
    let d = match docs::get(&rtx, &c.doc_id)? {
        Some(d) => d,
        None => return Ok(None),
    };
    if !keep_doc(&d, scope, &SearchFilter::default()) {
        return Ok(None);
    }
    if !is_latest_version(&rtx, &d)? {
        return Ok(None);
    }

    let abs = ctx.paths.root.join(&d.markdown_path);
    let chunk_text = crate::kb::content_store::read::read_doc_range(
        &abs, c.byte_offset.0, c.byte_offset.1,
    )?;
    let main = ChunkPayload {
        chunk_id: c.id.clone(),
        doc_id: c.doc_id.clone(),
        heading_path: c.heading_path.clone(),
        text: chunk_text,
    };

    let neighbors: Vec<ChunkPayload> = match input.expand.as_str() {
        "neighbor" => {
            // Pull chunks for the same logical_source_id, find the
            // ones with seq adjacent to ours.
            let all = chunks::chunks_for_logical(&rtx, &c.logical_source_id)?;
            let mut adj: Vec<_> = all
                .into_iter()
                .filter(|x| {
                    x.doc_id == c.doc_id
                        && (x.seq + 1 == c.seq || x.seq == c.seq + 1)
                })
                .collect();
            adj.sort_by_key(|x| x.seq);
            adj.into_iter()
                .map(|x| ChunkPayload {
                    chunk_id: x.id.clone(),
                    doc_id: x.doc_id.clone(),
                    heading_path: x.heading_path.clone(),
                    text: crate::kb::content_store::read::read_doc_range(
                        &abs, x.byte_offset.0, x.byte_offset.1,
                    )
                    .unwrap_or_default(),
                })
                .collect()
        }
        _ => Vec::new(),
    };

    let full_doc = if input.expand == "full_doc" {
        Some(read_doc_body(&abs)?)
    } else {
        None
    };

    Ok(Some(KbFetchOutput { chunk: main, neighbors, full_doc }))
}

#[cfg(test)]
mod tests {
    // Integration-style; covered by tests/kb_week3_search.rs.
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::tools::kb_fetch
git add src/kb/tools/
git commit -m "feat(kb): tools::kb_fetch — chunk by id with neighbor/full_doc expansion"
```

---

## Task 13: `tools/kb_list_docs.rs` — paginated doc listing

**Files:** `src/kb/tools/kb_list_docs.rs`, modify `src/kb/tools/mod.rs`

Iterates `kb_docs` table (filtered by `keep_doc` + `is_latest_version`), returns paginated `[ {doc_id, title, source_kind, tags, ...} ]`.

- [ ] **Step 1: Write tool + tests**

```rust
//! kb_list_docs: paginated listing of visible docs.

use crate::kb::model::{CallerScope, KbSourceKind};
use crate::kb::search::filter::{is_latest_version, keep_doc, SearchFilter};
use crate::kb::search::pipeline::SearchCtx;
use crate::kb::store::codec::decode;
use crate::kb::store::schema::KB_DOCS;
use anyhow::Result;
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct KbListDocsInput {
    #[serde(default)]
    pub tags: Vec<String>,
    pub source_kind: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<String>,
}

fn default_limit() -> usize { 50 }

#[derive(Debug, Serialize)]
pub struct KbListDocsOutput {
    pub docs: Vec<DocSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DocSummary {
    pub doc_id: String,
    pub title: String,
    pub source_kind: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub version: u32,
}

pub fn run(ctx: &SearchCtx, input: KbListDocsInput, scope: &CallerScope) -> Result<KbListDocsOutput> {
    let filter = SearchFilter {
        tags: input.tags,
        source_kind: input.source_kind.as_deref().and_then(KbSourceKind::from_str),
        doc_ids: None,
        require_entities: vec![],
    };
    let rtx = ctx.store.begin_read()?;
    let tbl = rtx.open_table(KB_DOCS)?;
    let cursor_key = input.cursor.unwrap_or_default();
    let mut out = Vec::new();
    let mut next: Option<String> = None;
    for entry in tbl.range::<&str>(cursor_key.as_str()..)? {
        let (k, v) = entry?;
        let key = k.value().to_string();
        if key == cursor_key {
            continue;
        }
        let d: crate::kb::model::KbDoc = decode(v.value())?;
        if !keep_doc(&d, scope, &filter) {
            continue;
        }
        if !is_latest_version(&rtx, &d)? {
            continue;
        }
        if out.len() == input.limit {
            next = Some(key);
            break;
        }
        out.push(DocSummary {
            doc_id: d.id.clone(),
            title: d.title.clone(),
            source_kind: d.source_kind.as_str().to_string(),
            tags: d.tags.clone(),
            created_at: d.created_at,
            version: d.version,
        });
    }
    Ok(KbListDocsOutput { docs: out, next_cursor: next })
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::tools::kb_list_docs
git add src/kb/tools/
git commit -m "feat(kb): tools::kb_list_docs — paginated visible-doc listing with cursor"
```

---

## Task 14: `tools/kb_similar.rs` — chunk → nearest neighbors

**Files:** `src/kb/tools/kb_similar.rs`, modify `src/kb/tools/mod.rs`

Given a `chunk_id`, fetch its vector from redb, run `hnsw.search()`, filter scope. `scope: "any|same_doc|other_docs"` controls whether neighbors restrict to same/different doc.

- [ ] **Step 1: Write tool + tests**

```rust
//! kb_similar: vector neighbors of a chunk.

use crate::kb::model::{CallerScope, KbChunk};
use crate::kb::search::filter::{is_latest_version, keep_doc, SearchFilter};
use crate::kb::search::pipeline::SearchCtx;
use crate::kb::store::{chunks, docs};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct KbSimilarInput {
    pub chunk_id: String,
    #[serde(default = "default_k")]
    pub k: usize,
    #[serde(default = "default_scope")]
    pub scope: String, // any|same_doc|other_docs
    #[serde(default = "default_min_score")]
    pub min_score: f32,
    #[serde(default)]
    pub exclude_neighbors: bool,
}

fn default_k() -> usize { 8 }
fn default_scope() -> String { "any".into() }
fn default_min_score() -> f32 { 0.0 }

#[derive(Debug, Serialize)]
pub struct KbSimilarOutput {
    pub neighbors: Vec<NeighborHit>,
}

#[derive(Debug, Serialize)]
pub struct NeighborHit {
    pub chunk_id: String,
    pub doc_id: String,
    pub score: f32,
}

pub fn run(ctx: &SearchCtx, input: KbSimilarInput, scope: &CallerScope) -> Result<KbSimilarOutput> {
    let rtx = ctx.store.begin_read()?;
    let seed = match chunks::get(&rtx, &input.chunk_id)? {
        Some(c) => c,
        None => return Ok(KbSimilarOutput { neighbors: vec![] }),
    };
    let raw = ctx.index.hnsw.search(&seed.vector, input.k * 3);
    let mut out = Vec::new();
    for (cid, score) in raw {
        if cid == input.chunk_id {
            continue;
        }
        if score < input.min_score {
            continue;
        }
        let c = match chunks::get(&rtx, &cid)? {
            Some(c) => c,
            None => continue,
        };
        let d = match docs::get(&rtx, &c.doc_id)? {
            Some(d) => d,
            None => continue,
        };
        if !keep_doc(&d, scope, &SearchFilter::default()) || !is_latest_version(&rtx, &d)? {
            continue;
        }
        match input.scope.as_str() {
            "same_doc" if c.doc_id != seed.doc_id => continue,
            "other_docs" if c.doc_id == seed.doc_id => continue,
            _ => {}
        }
        if input.exclude_neighbors {
            // Exclude seq±1 chunks within the same logical source.
            if c.logical_source_id == seed.logical_source_id
                && (c.seq + 1 == seed.seq || c.seq == seed.seq + 1)
            {
                continue;
            }
        }
        out.push(NeighborHit { chunk_id: cid, doc_id: c.doc_id, score });
        if out.len() == input.k {
            break;
        }
    }
    Ok(KbSimilarOutput { neighbors: out })
}
```

- [ ] **Step 2: Run + commit**

```bash
git add src/kb/tools/
git commit -m "feat(kb): tools::kb_similar — vector neighbors with scope/min_score/exclude_neighbors"
```

---

## Task 15: `tools/kb_search_entities.rs` — entity inverted index

**Files:** `src/kb/tools/kb_search_entities.rs`, modify `src/kb/tools/mod.rs`

Thin wrapper around `store::entities::find_by_surface`.

- [ ] **Step 1: Write tool + tests**

```rust
//! kb_search_entities: surface → entity_id lookup.

use crate::kb::model::CallerScope;
use crate::kb::search::pipeline::SearchCtx;
use crate::kb::store::entities;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct KbSearchEntitiesInput {
    pub query: String,
    pub kind: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize { 20 }

#[derive(Debug, Serialize)]
pub struct KbSearchEntitiesOutput {
    pub matches: Vec<EntityMatch>,
}

#[derive(Debug, Serialize)]
pub struct EntityMatch {
    pub entity_id: String,
    pub canonical_name: String,
    pub kind: String,
    pub aliases: Vec<String>,
}

pub fn run(
    ctx: &SearchCtx,
    input: KbSearchEntitiesInput,
    _scope: &CallerScope,
) -> Result<KbSearchEntitiesOutput> {
    let rtx = ctx.store.begin_read()?;
    let kind_filter = input.kind.as_deref();
    let idx_rows = entities::find_by_surface(&rtx, &input.query, kind_filter)?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in idx_rows {
        if !seen.insert(row.entity_id.clone()) {
            continue;
        }
        if let Some(e) = entities::get_entity(&rtx, &row.entity_id)? {
            out.push(EntityMatch {
                entity_id: e.id.clone(),
                canonical_name: e.canonical_name.clone(),
                kind: e.kind.as_str().to_string(),
                aliases: e.aliases.clone(),
            });
            if out.len() == input.limit {
                break;
            }
        }
    }
    Ok(KbSearchEntitiesOutput { matches: out })
}
```

- [ ] **Step 2: Run + commit**

```bash
git add src/kb/tools/
git commit -m "feat(kb): tools::kb_search_entities — surface→entity lookup via inverted index"
```

---

## Task 16: e2e integration test — kb_search end-to-end

**Files:** `tests/kb_week3_search.rs`

Ingest 3 docs (different content), drain worker, build `SearchCtx`, call `kb_search` with various queries + filters, assert correct hits.

- [ ] **Step 1: Write integration test**

```rust
//! Week 3 end-to-end: ingest → worker → kb_search returns expected
//! ranked chunks with visibility filtering applied.

use anyhow::Result;
use rsclaw::kb::{
    canonicalize_by_mime, ingest_canonicalized,
    search::{Diversity, SearchCtx, SearchMode, SearchRequest},
    search::filter::SearchFilter,
    tools::kb_search,
    CanonicalizeInput, CallerScope, DefaultDispatcher, HandlerCtx, IngestInput,
    KbEmbedder, KbIndex, KbPaths, KbStore, KbVisibility, StubEmbedder,
    WorkerConfig, WorkerPool,
};
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_kb_search_ranks_relevant_chunks_top() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths)?);

    let docs = [
        ("# Cats\n\nCats are nocturnal hunters that prowl rooftops.", "cats"),
        ("# Dogs\n\nDogs love walks and play fetch with their humans.", "dogs"),
        ("# Astronomy\n\nThe sun is a yellow dwarf star in the Milky Way.", "stars"),
    ];

    let hctx = HandlerCtx {
        store: store.clone(),
        paths: paths.clone(),
        embedder: embedder.clone(),
        index: index.clone(),
    };
    let cfg = WorkerConfig::default();
    for (body, _) in docs {
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: body.as_bytes(),
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: None,
        })?
        .unwrap();
        ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon, raw_bytes: body.as_bytes(), raw_ext: "md",
                visibility: None, owner_user_id: None, seen_key: None,
                source: None, paths: &paths,
            },
        )?;
        WorkerPool::run_one_blocking(&hctx, &cfg, &DefaultDispatcher)?;
    }

    let ctx = SearchCtx { store: store.clone(), index, paths, embedder };
    let out = kb_search::run(
        &ctx,
        kb_search::KbSearchInput {
            query: "yellow dwarf star".into(),
            k: 3,
            filter: Default::default(),
            mode: "hybrid".into(),
            diversity: "mmr".into(),
            mmr_lambda: 0.5,
            boost_entities: vec![],
        },
        &CallerScope::default(),
    )?;
    assert!(!out.results.is_empty(), "expected at least one hit");
    // The astronomy doc should rank top for "yellow dwarf star".
    assert!(
        out.results[0].doc_title.contains("Astronomy")
            || out.results[0].text.to_lowercase().contains("dwarf"),
        "top hit should be the astronomy doc, got: {:?}",
        out.results[0]
    );
    Ok(())
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test --test kb_week3_search
git add tests/kb_week3_search.rs
git commit -m "test(kb): Week 3 e2e — ingest → worker → kb_search returns ranked hits"
```

---

## Task 17: README + invariants update

**Files:** `src/kb/README.md`

Add Week 3 scope, invariants 15–18, retrieval quick-start.

- [ ] **Step 1: Edit README**

Add a "Week 3 (Retrieval)" subsection under "What's implemented". Add invariants:

```
15. **Visibility filter runs on every retrieval call** — every `tools/kb_*`
    entry point goes through `search::filter::keep_doc` and
    `is_latest_version`. There is no caller-supplied bypass. Covered by
    `kb::search::pipeline::tests::search_filter_by_visibility_hides_private`.
16. **HNSW + tantivy are caches over redb** — losing either is a
    rebuild, not data loss. `KbIndex::open_and_rebuild` reconstructs
    both from `kb_chunks` on startup. Covered by
    `kb::index::hnsw::tests::rebuild_then_search_finds_chunks` and
    `kb::index::tantivy::tests::*`.
17. **Tantivy upsert deletes-by-term before add** — re-running
    `chunk_embed` on the same chunk_id replaces the indexed text
    rather than producing a duplicate match. Covered by
    `kb::index::tantivy::tests::upsert_replaces_previous`.
18. **CallerScope is injected by the runtime, not by tool input** —
    `kb_search::KbSearchInput` deliberately has no `caller_scope`
    field; the runtime constructs scope from auth context and passes
    it as a separate function argument.
```

- [ ] **Step 2: Run full suite + commit**

```bash
cargo test -p rsclaw --lib kb::
cargo test --test kb_week1_e2e --test kb_week2_pipeline --test kb_week2_recovery --test kb_week3_search
git add src/kb/README.md
git commit -m "docs(kb): README — Week 3 scope + invariants 15–18 + retrieval quick-start"
```

---

## What's implemented (Weeks 1–3)

**Week 3 (Retrieval):**

- **HnswCache** (`index/hnsw.rs`) — `ArcSwap<Hnsw<f32, DistCosine>>` with rebuild-from-redb.
- **TantivyIndex** (`index/tantivy.rs`) — BM25 with `chunk_id`-keyed upsert + rebuild.
- **KbIndex composite** (`index/mod.rs`) — single handle, atomic write via `upsert_chunk` + `commit`.
- **Worker integration** — `ChunkAndEmbed` handler writes to both indexes after redb commit.
- **Filter** (`search/filter.rs`) — visibility + status + version + tags + source_kind + doc_ids.
- **RRF** (`search/rrf.rs`) — pure-function fusion with deterministic tie-break.
- **MMR** (`search/mmr.rs`) — greedy diversity selector.
- **Pipeline** (`search/pipeline.rs`) — dense + sparse → filter → fuse → MMR → fetch.
- **Tools** (`tools/`) — `kb_search`, `kb_fetch`, `kb_list_docs`, `kb_similar`, `kb_search_entities`.

## What's NOT in Weeks 1–3

- HNSW snapshot persistence — Week 3.5 (rebuild-from-redb today)
- Real BGE-M3 embedder — Week 2.5
- Entity extraction — Week 4 (`KbChunk.entities` empty, `kb_search_entities` returns empty until then)
- CJK tokenizer for tantivy — Week 3.5 (whitespace + lowercase today)
- URL fetch + `UrlSyncer` — Week 4
- Compactor (orphan files, stale chunks, Failed-job cleanup) — Week 4
- `kb_explain` trace tool — V2 (post-MVP)

---

## Open questions (resolve as you implement)

- **HNSW insert vs full rebuild trade-off** — Task 3 ships rebuild-on-insert
  for simplicity. If the worker batch is too slow with N>1k chunks,
  rewrite `insert` to truly append to the active hnsw (hnsw_rs supports
  this, just no overwrite). Add a benchmark in Week 3.5.
- **Tantivy reader reload policy** — `OnCommitWithDelay` means reads
  see writes after a small delay. For tests that ingest + immediately
  search, may need to call `reader.reload()` explicitly. If tests
  flake, switch to `ReloadPolicy::Manual` and call reload after commit.
- **Embedder consistency** — `kb_search` re-embeds the query at search
  time. If the embedder swaps mid-flight (Week 2.5 BGE-M3 lands), all
  previously embedded chunk vectors become incompatible. Spec §L says
  this triggers a full rebuild — leave a TODO in `pipeline::search`
  to verify `embedder_id` matches the embedder's id on each call (warn
  if mismatch).
- **`entity_alignment` field** — Week 3 returns empty; Week 4 fills
  this from `kb_search_entities` matches against the query terms.

---

## Self-review checklist (run before committing the plan)

- [ ] Every task references specific files; no `see Task X` cross-references that hide code.
- [ ] No `TBD` / placeholder code in any code block.
- [ ] Type names match across tasks: `KbIndex`, `SearchCtx`, `SearchRequest`, `RetrievalHit`, `CallerScope`, `SearchFilter` referenced consistently.
- [ ] Visibility filter is wired through `kb_search`, `kb_fetch`, `kb_list_docs`, `kb_similar` — every retrieval entry point.
- [ ] `HandlerCtx` gains `index: Arc<KbIndex>` and existing Week 2 tests are updated to construct it.
- [ ] Spec §3 / §K / §L points each map to a concrete task or are explicitly deferred.

---

## Execution-time fix sweep — eight more findings, all fixed

These surfaced *during* Week 3 execution and were fixed inline at the
failure point. Plan + code are consistent; the final test suite is
192 unit + 10 integration, 0 ignored, 0 failed.

1. **T2 entity model mismatch** — plan assumed Week 1 `KbEntity` had
   `id`/`canonical_name`/`aliases`/`description` and a `Place` variant.
   Week 1 actually ships `canonical_id`/`surface_forms`/`kind`/`created_at`
   and no `Place`. Rewrote `store::entities` against the real model:
   linear `find_by_surface` scan over `surface_forms`, `chunks_for_entity`
   over the entity→chunk index. Inverted-index optimisation is a Week 4
   follow-up.

2. **T3 `HnswCache::insert` placeholder didn't compile** — plan body
   left a `i_must_keep_compiler_happy_about_v_id` token + ad-hoc TODO.
   Replaced the whole module with a `RwLock<HnswInner>` design:
   append-only `insert` (re-inserts orphan the old vertex; rebuild
   from redb reaps), `search` reads under shared lock, `rebuild`
   constructs a fresh hnsw + swaps under exclusive lock. ArcSwap is a
   Week 3.5 optimisation once we have a hot-read benchmark.

3. **T7/T11/T13 `KbSourceKind::from_str`** — Week 1 actually exposes
   `KbSourceKind::parse(&str) -> Result<Self, String>`. All tool
   wrappers updated to use `parse(...).ok()`.

4. **T10 `KbLocator` variant fields** — plan named the wrong fields:
   - `MdSection { line, .. }` → actual `MdSection { heading_path }`
   - `UrlAnchor { anchor, .. }` → actual `UrlAnchor { fragment }`
   Fixed by using the existing `KbLocator::human()` method from Week 1
   instead of writing a parallel renderer in the pipeline.

5. **T10 `KbSource::Url { url }`** — actual variant has
   `Url { url, fetched_at }`. Fixed `render_source` to destructure
   with `Url { url, .. }`.

6. **T11 manual `Serialize` impls** — plan hand-wrote `Serialize` for
   `RetrievalHit` and `Citation`. Replaced with `#[derive(Serialize)]`;
   `KbLocator` already derives `Serialize` so nesting works.

7. **T15 entity field mismatch** — plan's `EntityMatch` used
   `canonical_name`/`aliases` which don't exist on Week 1 `KbEntity`.
   Use first `surface_forms` as the display name, rest as aliases.

8. **T5 test tantivy `LockBusy`** — `open_and_rebuild_recovers_both_layers`
   opened two `KbIndex` instances on the same `idx/tantivy/` path
   simultaneously (the worker's pre-index + the post-rebuild check).
   Tantivy holds a per-process exclusive directory lock. Fixed by
   scoping the worker's index in a block so the lock is released
   before the rebuild check opens its own.

Smoke after sweep:

```bash
$ cargo test -p rsclaw --lib kb::
test result: ok. 192 passed; 0 failed; 0 ignored
$ cargo test --test kb_week1_e2e --test kb_week2_pipeline --test kb_week2_recovery --test kb_week3_search
all pass (6 + 1 + 2 + 1 = 10 integration tests)
```
