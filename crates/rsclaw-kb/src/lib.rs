//! Knowledge base module — user-facing RAG over local docs.
//!
//! Design: `docs/specs/2026-05-19-knowledge-base.md`
//! ADR:    `docs/adr/0001-knowledge-base.md`
//! Week plans: `docs/plans/2026-05-19-kb-mvp-week{1..4}-*.md`
//! README:  `src/kb/README.md` (invariants 1–28)
//!
//! Layout:
//!   model/         — KbDoc, KbChunk, KbEntity, LogicalSourceId, KbVisibility,
//! …   content_store/ — atomic md/<kind>/<slug>--<lsid8>--<md8>.md writer +
//! readers   store/         — redb schema (13 tables) + per-table accessors
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

pub mod canonicalize;
pub mod chunker;
pub mod compactor;
pub mod content_store;
pub mod embedder;
pub mod entities;
pub mod index;
pub mod jobs;
pub mod ledger;
pub mod model;
pub mod paths;
pub mod pipeline;
pub mod search;
pub mod rerank;
pub mod service;
pub mod store;
pub mod sync;
pub mod tools;
pub mod util;
pub mod worker;

// Public façade — re-export the surface most callers need so they
// can `use rsclaw::kb::{stage_doc, chunk_markdown, ...}` without
// reaching into submodules. Submodules stay `pub` for advanced
// callers that need finer control.

pub use canonicalize::{
    CanonicalMetadata, CanonicalizeInput, CanonicalizedSource, canonicalize_by_mime,
    canonicalize_url, detect_mime,
};
pub use chunker::{ChunkerInput, LocatorKind, chunk_markdown};
pub use content_store::{
    FrontMatter, StageInput, StagedDoc, compose_doc_file, parse_doc_file, read_doc_body,
    read_doc_range, stage_doc, verify_doc_sha,
};
pub use embedder::{KbEmbedder, StubEmbedder};
pub use index::{HnswCache, KbIndex, TantivyIndex};
pub use jobs::{ClaimToken, Job, JobKind, JobStatus};
pub use ledger::{IngestLedgerEntry, LedgerOp, LedgerStatus};
pub use model::{
    CallerScope, ChunkStatus, EntityKind, KbChunk, KbDoc, KbEntity, KbEntityIndex, KbLocator,
    KbSource, KbSourceKind, KbStatus, KbVisibility, LogicalSourceId, MailSource, VersionPointer,
    chunk_id, hamming64, simhash64,
};
pub use paths::KbPaths;
pub use pipeline::{IngestInput, IngestOutput, ingest_canonicalized};
pub use service::{KnowledgeError, KnowledgeService};
pub use store::{KbStore, open_db};
pub use util::{RAG_DISCIPLINE_PROMPT, redact};
pub use worker::{DefaultDispatcher, HandlerCtx, JobHandler, WorkerConfig, WorkerPool};

// ---------------------------------------------------------------------------
// Process-global handle to the live KnowledgeService.
//
// The gateway opens exactly ONE KnowledgeService at startup (redb is
// single-writer / exclusively locked — a second open would fail, cf. the
// /memory live-store reuse fix). Agent tools need that same instance without
// threading an Arc through the spawner + runtime constructors, so startup
// registers it here and the `knowledge_base` tool reads it. None when KB
// failed to open (gateway stays up; the tool reports unavailable).
// ---------------------------------------------------------------------------
static GLOBAL_SERVICE: std::sync::OnceLock<std::sync::Arc<KnowledgeService>> =
    std::sync::OnceLock::new();

/// Register the live KnowledgeService. Called once by gateway startup.
pub fn set_global_service(svc: std::sync::Arc<KnowledgeService>) {
    let _ = GLOBAL_SERVICE.set(svc);
}

/// Reach the live KnowledgeService, if the gateway opened one.
pub fn global_service() -> Option<std::sync::Arc<KnowledgeService>> {
    GLOBAL_SERVICE.get().cloned()
}
