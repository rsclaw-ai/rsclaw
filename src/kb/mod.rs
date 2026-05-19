//! Knowledge base module — user-facing RAG over local docs.
//!
//! Design: `docs/specs/2026-05-19-knowledge-base.md`
//! ADR:    `docs/adr/0001-knowledge-base.md`
//! Week 1 plan: `docs/plans/2026-05-19-kb-mvp-week1-foundation.md`
//!
//! Layout:
//!   model/         — KbDoc, KbChunk, KbEntity, LogicalSourceId, KbVisibility, …
//!   content_store/ — atomic md/<kind>/<slug>--<lsid8>.md writer + readers
//!   store/         — redb schema (13 tables) + accessors (Week 2+)
//!   canonicalize/  — text/md/html/pdf → Markdown; url string canonicalization
//!   chunker/       — markdown → KbChunk[] with deterministic chunk_id
//!   ledger/        — IngestLedger + Outbox types (accessors Week 2+)
//!   jobs/          — Job queue types (accessors Week 2+)
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
pub mod embedder;
pub mod pipeline;
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
