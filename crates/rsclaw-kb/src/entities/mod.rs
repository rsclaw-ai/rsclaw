//! Regex-based entity extraction (v1).
//!
//! Spec §2 ("Entity Extraction") describes a more sophisticated v2
//! pipeline (NER model + canonicalisation across mentions). Week 4.5
//! ships a regex-only extractor that covers the high-value mention
//! classes — URLs, emails, hashtags, @-mentions — so the
//! `boost_entities` / `require_entities` retrieval filters and the
//! `kb_search_entities` tool become non-trivially useful.
//!
//! The output of `extract_entities` is a list of `(kind, surface)`
//! tuples; callers (the worker handler) are responsible for
//! upserting `KbEntity` + `KbEntityIndex` edges through the
//! `store::entities` accessors.

pub mod extract;

pub use extract::{ExtractedMention, extract_entities};
