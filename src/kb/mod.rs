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
pub mod util;
