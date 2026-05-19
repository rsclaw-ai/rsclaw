//! Knowledge base module — user-facing RAG over local docs.
//!
//! Design: `docs/specs/2026-05-19-knowledge-base.md`
//! ADR:    `docs/adr/0001-knowledge-base.md`
//! Week plans: `docs/plans/2026-05-19-kb-mvp-week{1..4}-*.md`
//! README:  `src/kb/README.md` (invariants 1–28)
//!
//! Layout:
//!   model/         — KbDoc, KbChunk, KbEntity, LogicalSourceId, KbVisibility, …
//!   content_store/ — atomic md/<kind>/<slug>--<lsid8>--<md8>.md writer + readers
//!   store/         — redb schema (13 tables) + per-table accessors
//!   canonicalize/  — text/md/html/pdf → Markdown; url string canonicalization
//!   chunker/       — markdown → KbChunk[] with deterministic chunk_id
//!   ledger/        — IngestLedger + Outbox types
//!   jobs/          — Job queue types (state machine + fencing tokens)
//!   embedder/      — KbEmbedder trait + StubEmbedder (BGE-M3 deferred)
//!   pipeline/      — ingest_canonicalized: single-tx atomic ingest
//!   worker/        — WorkerPool drains ChunkAndEmbed jobs (tokio)
//!   index/         — HnswCache + TantivyIndex composite (CJK tokenizer,
//!                    snapshot persistence)
//!   search/        — filter + RRF + MMR + pipeline (visibility-safe)
//!   tools/         — kb_search / kb_fetch / kb_list_docs / kb_similar /
//!                    kb_search_entities (JSON-shaped MCP wrappers)
//!   entities/      — regex entity extractor (URLs/emails/hashtags/mentions)
//!   sync/          — KbSourceSyncer trait + ManualUpload + UrlSyncer
//!   compactor/     — orphan file scan + ledger state advancement
//!   util/          — redact() for PII-safe logging
//!   paths.rs       — KbPaths resolves ~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/

pub mod paths;
pub mod model;
pub mod content_store;
pub mod store;
pub mod canonicalize;
pub mod chunker;
pub mod ledger;
pub mod jobs;
pub mod compactor;
pub mod embedder;
pub mod entities;
pub mod index;
pub mod pipeline;
pub mod search;
pub mod sync;
pub mod tools;
pub mod worker;
pub mod util;

// Public façade — re-export the surface most callers need so they
// can `use rsclaw::kb::{stage_doc, chunk_markdown, ...}` without
// reaching into submodules. Submodules stay `pub` for advanced
// callers that need finer control.

pub use paths::KbPaths;
pub use model::{
    chunk_id, hamming64, simhash64, CallerScope, ChunkStatus, EntityKind, KbChunk, KbDoc,
    KbEntity, KbEntityIndex, KbLocator, KbSource, KbSourceKind, KbStatus, KbVisibility,
    LogicalSourceId, MailSource, VersionPointer,
};
pub use content_store::{
    compose_doc_file, parse_doc_file, read_doc_body, read_doc_range, stage_doc,
    verify_doc_sha, FrontMatter, StageInput, StagedDoc,
};
pub use canonicalize::{
    canonicalize_by_mime, canonicalize_url, detect_mime, CanonicalMetadata, CanonicalizeInput,
    CanonicalizedSource,
};
pub use chunker::{chunk_markdown, ChunkerInput, LocatorKind};
pub use store::{open_db, KbStore};
pub use ledger::{IngestLedgerEntry, LedgerOp, LedgerStatus};
pub use jobs::{ClaimToken, Job, JobKind, JobStatus};
pub use util::redact;
pub use embedder::{KbEmbedder, StubEmbedder};
pub use pipeline::{ingest_canonicalized, IngestInput, IngestOutput};
pub use worker::{DefaultDispatcher, HandlerCtx, JobHandler, WorkerConfig, WorkerPool};
pub use index::{HnswCache, KbIndex, TantivyIndex};
