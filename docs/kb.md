# Knowledge Base — managed RAG with collections + OOXML ingest

A first-class persistent knowledge store, separate from session memory. Use for project docs, reference material, codebases, meeting notes, legal contracts — anything you want the agent to cite rather than summarize from training.

For the user-facing pitch see [README](../README.md#knowledge-base--managed-rag-ooxml-in-snippets-out). For the engineering deep-dive on the ingest pipeline / worker pool / redb schema, see [`src/kb/README.md`](../src/kb/README.md). This doc is the operations manual.

---

## What it is, what it isn't

**Is**: a deterministic ingest pipeline (dedupe → canonicalize → chunk → embed → index → cite), a hybrid retriever (BM25 + vector + RRF + MMR), per-collection tagging, durable redb store, drag-drop ingest UI, agent tools that return snippets with `doc_id` + offset so replies can quote.

**Isn't**: an arbitrary file mirror or document workspace. Files are canonicalized to text + metadata at ingest time; the original isn't re-served. There's also no granular per-user ACL inside a collection — visibility is collection-level.

KB and memory complement each other:

| | Memory | Knowledge Base |
|---|---|---|
| What | extracted user signal | ingested documents |
| Lifetime | self-pruning (Weibull decay) | sticks until you delete |
| Citation | not first-class | first-class (`doc_id` + offsets returned) |
| Source | extracted from chat | files / URLs / dirs |
| Scope | per-agent | per-collection (tag veneer) |

---

## Collections

A collection is a **tag veneer over a single shared embedding index** — there isn't a separate `Hnsw` per collection. This keeps memory tight, makes cross-collection search cheap, and means "create a collection" is a sub-millisecond op.

```bash
rsclaw knowledge collections list
rsclaw knowledge collections create "会议记录"
rsclaw knowledge collections delete <id> --yes
```

Or via HTTP:

```
GET    /api/v1/knowledge/collections                    list
POST   /api/v1/knowledge/collections                    create   { name, description? }
GET    /api/v1/knowledge/collections/{id}               read
PATCH  /api/v1/knowledge/collections/{id}               rename / re-describe
DELETE /api/v1/knowledge/collections/{id}               delete (cascades doc tag removal)
```

Names are case-insensitive unique; concurrent creates race-safely (duplicate-name error returns the existing one).

---

## Ingest

### File / dir / URL

```bash
# Local file (canonicalized by MIME; OOXML and PDF supported)
rsclaw knowledge ingest ./Q3-report.pdf --collection 财报
rsclaw knowledge ingest ./quarterly.docx --collection 财报
rsclaw knowledge ingest ./spreadsheet.xlsx --collection 财报

# Whole directory — recursive, supported file types only
rsclaw knowledge ingest ./company-docs/ --collection 公司

# From URL (ETag / Last-Modified aware; falls back to content-hash dedupe)
rsclaw knowledge ingest --url https://example.com/whitepaper.pdf --collection 行业研究
```

HTTP equivalents:

```
POST /api/v1/knowledge/collections/{id}/docs            multipart upload
POST /api/v1/knowledge/collections/{id}/docs/from-url   { url }
POST /api/v1/knowledge/collections/{id}/docs/from-path  { path }
POST /api/v1/knowledge/collections/{id}/docs/from-dir   { path }
```

### What "supported" means

The canonicalizer (`src/kb/canonicalize/`) routes by MIME to a per-format extractor:

- **Markdown / plain text** — passes through
- **HTML** — lol-html based text extraction
- **PDF** — text-layer extraction (no OCR yet)
- **OOXML** — `.docx` / `.xlsx` / `.pptx` extracted via crate-side parsers, preserving paragraph and slide structure
- **JSON / YAML / TOML** — pretty-printed as text
- **Source code** — language-aware comment + signature extraction (Tree-sitter)

Other MIME types are skipped with a `Failed(unsupported)` ledger entry; nothing crashes.

### Determinism + dedupe

Every doc has a `doc_id` derived from `(source_kind, source_uri)` and a `doc_version` derived from content hash. Re-ingesting the same file is a NOOP (fast path on `seen_items` table); a content change writes a new version and the chunker drops stale chunks of prior versions before inserting the new set. This is all in a single redb `WriteTransaction`, so partial failures don't leave half-indexed docs.

### Pipeline stages (per doc)

```
upload → stage → canonicalize → write doc+ledger+chunks(empty)+job → commit
                                                                       │
                                                              worker claims job
                                                                       │
                                                       chunk + embed + tantivy upsert
                                                                       │
                                                       ledger: IndexingComplete
                                                                       │
                                                       compactor (orphan scan + grace)
                                                                       │
                                                                ledger: Done
```

The pool single-worker design (`src/kb/worker/`) means at-most-one chunker is running per process; jobs are durable across restarts (claim fencing token guards against zombie workers). The desktop app's progress bar reads the ledger state directly.

---

## Search

### From the agent (auto-cite)

The `knowledge_base` tool is in the standard toolset. When the agent's query semantically matches a collection, it calls:

```
kb_search(query, collection?, top_k?)  → [{ chunk_id, doc_id, text, score, ... }]
kb_fetch(doc_id, offset?, length?)     → raw doc snippet
kb_list_docs(collection?)              → doc list with metadata
kb_similar(doc_id)                     → nearest-doc neighbors
kb_search_entities(query)              → entity-scoped search
```

Snippets carry `doc_id`, byte/char offsets, and source metadata so replies cite cleanly (`"根据 Q3 财报 第 12 页 ..."`).

### From the CLI

```bash
rsclaw knowledge search "Q3 毛利率"             # search across all collections
rsclaw knowledge search "Q3 毛利率" --collection 财报
rsclaw knowledge show <doc_id>                  # full doc snippet
rsclaw knowledge stats                          # docs/collections/chunks/embeddings
rsclaw knowledge compact                        # orphan-chunk reaper
```

### From HTTP

```
POST /api/v1/knowledge/search                   { query, collection?, top_k? }
GET  /api/v1/knowledge/stats
GET  /api/v1/knowledge/embedders                list configured embedders + status
GET  /api/v1/knowledge/events                   SSE: ingest progress stream
```

### Retrieval pipeline

`SearchCtx::search` (`src/kb/search/pipeline.rs`) composes:

1. **Dense** — hnsw_rs cosine-distance nearest (top 4·k)
2. **Sparse** — tantivy BM25 with `JiebaTokenizer` for CJK (top 4·k)
3. **Filter** — visibility + status + version + tags + source_kind + doc_ids (single source of truth in `src/kb/search/filter.rs`)
4. **RRF** — reciprocal-rank fusion (pure function, `src/kb/search/rrf.rs`)
5. **MMR** — maximal marginal relevance for diversity (`src/kb/search/mmr.rs`)
6. **Lazy text fetch** — only the top-k pull text from `content_store`

The `JiebaTokenizer` (`src/kb/index/cjk.rs`) is registered as tantivy's `cjk` analyzer and applied to `indexed_text`, so Chinese queries hit Chinese chunks. ASCII queries round-trip identically.

---

## Embedders

Same trait + same model choices as memory — see [`memory.md`](memory.md#embedders) for the embedder section.

KB-specific notes:

- `KbEmbedder` and the runtime `Embedder` share the underlying `crate::embed::*` infrastructure, so swapping local ↔ remote is a config change with no re-ingest needed.
- A `StubEmbedder` exists for tests (deterministic 1024-d vectors).

Endpoint to inspect what's loaded:

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:18888/api/v1/knowledge/embedders | jq .
```

---

## Storage layout

Everything lands under `~/.rsclaw/var/data/kb/`:

```
kb.redb              docs / chunks / ledger / jobs / seen / entities / sync_state tables
content/             staged + canonicalized source files (gc'd by compactor)
hnsw.idx             on-disk vector index (rebuilt at startup from redb)
tantivy/             BM25 index
```

`hnsw.idx` is rebuildable — delete it and the next start reconstructs from redb. `tantivy/` is delete-then-add per-chunk-id, so reindex is a `--reindex` flag on the doc (`POST /api/v1/knowledge/collections/{id}/docs/{doc_id}/reindex`).

---

## Operating tips

**Compact regularly**

The compactor reaps orphan content-store files and advances `IndexingComplete → CleanupPending → Done`. `rsclaw knowledge compact` is idempotent; a cron job (e.g. nightly) is fine.

**One collection per project**

Cross-collection search is cheap, so be liberal with collections. Tagging at ingest time is much harder to undo than at query time.

**Watch the events stream**

For desktop / TUI integrations, subscribe to `/api/v1/knowledge/events` for ingest progress events. The desktop app does this — the same SSE stream is usable from any HTTP client.

**Reindex after embedder swap (not strictly required)**

Old vectors stay; new docs use the new embedder. If quality differs noticeably you can `POST /docs/{id}/reindex` per doc, or use the CLI `rsclaw knowledge reindex <doc_id>`.

**Pre-canonicalize for large corpora**

For tens of thousands of docs, pre-stage them to markdown / plain text first (e.g. with `pandoc`) and ingest from a directory of `.md` files. The OOXML / PDF canonicalizers are robust but not the fastest path.

---

## What's wired vs in-progress

Shipped (Weeks 1–5):

- ✅ Single-tx atomic ingest pipeline (dedupe → canonicalize → chunk → embed → index)
- ✅ OOXML (`.docx` / `.xlsx` / `.pptx`), PDF, HTML, Markdown, source code canonicalizers
- ✅ Hybrid BM25 (tantivy + JiebaTokenizer) + vector (hnsw_rs) + RRF + MMR retrieval
- ✅ Collections as tag veneer
- ✅ HTTP CRUD + SSE events
- ✅ Desktop drag-drop ingest UI (`/api/v1/knowledge/...`)
- ✅ Manual / URL / Dir syncers
- ✅ Compactor with grace period
- ✅ CLI parity (`rsclaw kb add | ls | rm | search | show | visibility | compact | stats`)
- ✅ Entity store (basic; inverted index pending)

In-progress / next:

- Inverted index on entities (Week 4+ optimisation, gated on real entity counts)
- More syncers (Notion, Google Drive, GitHub via webhook)
- Per-doc version history view in desktop UI
- PDF text-layer-less OCR fallback
