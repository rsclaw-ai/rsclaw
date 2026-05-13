//! /watch — live event stream → chat slash command.
//!
//! See `docs/superpowers/specs/2026-05-13-watch-design.md` for the design.

pub mod dedup;
pub mod parser;
pub mod rate_limit;
pub mod filter;
pub mod source;
pub mod delivery;

// Re-exported types and entry points are added in Task 16 (Registry).
