//! A-share market data via the upstream `astock` Go gateway
//! (https://github.com/oopos/astock). Wraps the `/v1/*` HTTP surface
//! with a typed Rust client, hot in-process TTL caches for the
//! repeat-heavy quote / kline / snapshot endpoints, and a global
//! `Arc` accessor mirroring `kb::global_service()` so non-runtime
//! code (CLI subcommands, LLM tool dispatch, cron triggers) can
//! reach the same client without plumbing parameters.
//!
//! Astock owns the hard parts — pytdx integration with the public
//! 通达信 servers, DuckDB-backed historical K-lines, iwencai
//! natural-language wrapping, x402 paywall billing. rsclaw is a
//! thin client that adds: code normalization, stock_name fill-in,
//! IM-friendly rendering, watchlist persistence (via memory store),
//! and cron-triggered briefings.
//!
//! Always-optional. When `config.astock.enabled` is false (or the
//! whole block is missing), `global_client()` returns `None` and
//! callers degrade to a structured "astock not configured" reply.

pub mod briefing;
pub mod chart;
pub mod client;
pub mod sse;

pub use client::{
    AstockClient, AstockError, AskResp, KlineBar, KlineResp, Quote, QuoteLevel, SnapshotRow,
};

/// Process-wide handle to the live astock HTTP client. Set once by
/// `gateway/startup.rs` when `config.astock.enabled` is true; read
/// by built-in LLM tools, `rsclaw stock` CLI, and any future cron
/// trigger that needs market data. `OnceLock` so reads are
/// lock-free and the set-then-read race is safe.
static GLOBAL_CLIENT: std::sync::OnceLock<std::sync::Arc<AstockClient>> = std::sync::OnceLock::new();

/// Register the singleton client. Idempotent — subsequent calls are
/// silently ignored (matches `kb::set_global_service`). Called from
/// gateway startup once the config has been resolved.
pub fn set_global_client(client: std::sync::Arc<AstockClient>) {
    let _ = GLOBAL_CLIENT.set(client);
}

/// Read accessor for the live client. Returns `None` when astock
/// is disabled or unconfigured — callers MUST handle this rather
/// than unwrap; "astock dormant" is a normal state for users who
/// don't trade A股.
pub fn global_client() -> Option<std::sync::Arc<AstockClient>> {
    GLOBAL_CLIENT.get().cloned()
}
