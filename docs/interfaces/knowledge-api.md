# Knowledge Base HTTP API — `/api/v1/knowledge/*`

Source of truth: `src/server/knowledge.rs` (this doc is reverse-engineered
from the implementation on `worktree-feat+knowledge-base`, 2026-05-21).
There was no pre-existing HTTP API spec — the original KB spec
(`docs/specs/2026-05-19-knowledge-base.md`) covers internal architecture
(ingest, ledger, UrlSyncer), not the wire contract.

## Conventions

- **Base path:** `/api/v1/knowledge`, mounted only when the KB store opened
  (`AppState.knowledge = Some(..)`); otherwise these routes 404.
- **Auth:** all routes sit behind the gateway `auth_middleware` — send
  `Authorization: Bearer <gateway.auth.token>`. No per-route bypass.
- **Field casing:** all JSON is `camelCase`.
- **Error envelope:** `{ "error": "<stable_code>" }` with an HTTP status.
  Internal errors return `500 {"error":"internal"}` (detail is logged, not
  returned).
- **Upload size cap:** `kb.maxDocMb` (default 50 MB), applied as the request
  body limit and enforced again per-handler.
- **Collections are a tag veneer** over a single KB store (no per-collection
  store/embedder); a collection is metadata + a `collection:<id>` tag on docs.
- **Consumer status:** the desktop UI does not call these endpoints yet
  (backend-first). No frontend contract to reconcile against.

## Collections

### `GET /collections`
→ `200 { "collections": [CollectionDto] }`

### `POST /collections`
Body: `{ "name": string, "description"?: string, "embedModel"?: string }`
- `400 {"error":"name_required"}` — name empty/whitespace
- `400 {"error":"name_too_long"}` — name > 100 chars
- `409 {"error":"duplicate_name"}`
- `201 CollectionDto`

### `GET /collections/{id}`
→ `200 CollectionDto` | `404 {"error":"collection_not_found"}`

### `PATCH /collections/{id}`
Body: `{ "name"?: string, "description"?: string }`
Absent fields are left unchanged. (Limitation: a present-but-null
`description` does not clear it — clearing is a future refinement.)
→ `200 CollectionDto` | `404 collection_not_found`

### `DELETE /collections/{id}`
Cascades to the collection's documents.
→ `200 { "deletedDocs": <u64> }` | `404 collection_not_found`

### CollectionDto
```jsonc
{
  "id": "col_...",
  "name": "手册",
  "description": null,
  "embedModel": null,
  "embedDim": 0,      // placeholder — always 0 today (P2 will populate)
  "docCount": 0,      // placeholder — always 0 today (P2)
  "chunkCount": 0,    // placeholder — always 0 today (P2)
  "bytes": 0,         // placeholder — always 0 today (P2)
  "createdAt": "2026-05-21T...Z",  // RFC3339
  "updatedAt": "2026-05-21T...Z"
}
```
> Note: `embedDim`/`docCount`/`chunkCount`/`bytes` are hardcoded `0` in the
> current build. Use `GET /stats` for real aggregate counts.

## Documents

### `GET /collections/{id}/docs`
→ `200 { "docs": [DocDto], "nextCursor": null }` (pagination not implemented;
cursor is always null) | `404 collection_not_found`

### `POST /collections/{id}/docs` — upload (async, returns 202)
Two content types; both canonicalize on the backend:

**JSON** (`application/json`) — text/markdown:
`{ "title": string, "text": string, "mime"?: string, "source"?: string }`
(`source` is accepted but currently ignored.)

**Multipart** (`multipart/form-data`) — binary (pdf/docx/xlsx/pptx):
fields `title` (optional; falls back to the uploaded filename) and `file`.
MIME is detected from the title/extension (mime is not trusted here).

Responses:
- `202 { "id", "title", "status": "pending", "bytes" }` — indexing runs in
  the background; poll `GET …/docs` or subscribe to `GET …/events`.
- `400` codes: `invalid_json`, `empty_content`, `title_required`,
  `body_too_large`, `invalid_multipart`
- `404 collection_not_found`

### `GET /collections/{id}/docs/{doc_id}`
→ `200 DocDto` | `404 doc_not_found` / `collection_not_found`

### `GET /collections/{id}/docs/{doc_id}/content`
→ `200` raw canonicalized body, `Content-Type: <mime>; charset=utf-8`
| `404 doc_not_found`

### `DELETE /collections/{id}/docs/{doc_id}`
→ `200 { "deleted": true }` | `404`

### `POST /collections/{id}/docs/{doc_id}/reindex`
→ `202 { "status": "indexing" }` | `404`

### DocDto
```jsonc
{
  "id": "doc_...",
  "title": "a.md",
  "source": "uploaded",          // always the literal "uploaded"
  "mime": "text/markdown",
  "bytes": 123,
  "chunkCount": 4,
  "status": "ready",             // "indexing" until ≥1 chunk exists, then "ready"
  "indexedAt": "2026-...Z",      // null while still indexing
  "createdAt": "2026-...Z"
}
```

## Search / stats / embedders / events

### `POST /search`
Body:
```jsonc
{
  "query": string,               // 1..=512 chars after trim
  "collectionIds"?: [string],    // empty = search all
  "topK"?: number,               // default 10, clamped 1..=50
  "scoreThreshold"?: number      // default 0.0
}
```
- `400 {"error":"invalid_query"}` — empty or > 512 chars
- `200 { "hits": [SearchHit], "queryMs": <u64> }`

SearchHit:
```jsonc
{
  "docId": "doc_...",
  "collectionId": "col_..." | null,
  "collectionName": "手册" | null,
  "sourceTitle": "a.md",
  "chunkText": "…",
  "score": 0.87
}
```

### `GET /stats`
→ `200 { "collectionCount", "docCount", "chunkCount", "bytes" }`

### `GET /embedders`
→ `200 { "default": "<id>"|null, "available": [{ "id", "label", "dim", "downloaded" }] }`

### `GET /events` — SSE
`text/event-stream`; each event `data` is JSON:
`{ "type": "knowledge.doc.status_changed", "docId": "...", "status": "ready" }`
Keep-alive `ping` every 15s. Lets the UI react to async indexing completing
without polling.

## Endpoint summary

| Method | Path | Purpose |
|---|---|---|
| GET | `/collections` | list collections |
| POST | `/collections` | create collection |
| GET | `/collections/{id}` | get collection |
| PATCH | `/collections/{id}` | update name/description |
| DELETE | `/collections/{id}` | delete (cascades to docs) |
| GET | `/collections/{id}/docs` | list docs |
| POST | `/collections/{id}/docs` | upload (JSON or multipart), 202 |
| GET | `/collections/{id}/docs/{doc_id}` | get doc metadata |
| GET | `/collections/{id}/docs/{doc_id}/content` | raw body |
| DELETE | `/collections/{id}/docs/{doc_id}` | delete doc |
| POST | `/collections/{id}/docs/{doc_id}/reindex` | re-enqueue indexing, 202 |
| POST | `/search` | semantic + BM25 hybrid search |
| GET | `/stats` | aggregate counts |
| GET | `/embedders` | available embedders |
| GET | `/events` | SSE doc-status stream |
