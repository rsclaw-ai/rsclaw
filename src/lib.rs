//! rsclaw library crate — a thin re-export facade.
//!
//! Everything moved out into workspace crates during the crate-split. This
//! facade re-exports them under their historical `rsclaw::<module>` paths so
//! the integration tests in `tests/*.rs` (and any embedder) keep resolving
//! `rsclaw::server::`, `rsclaw::gateway::`, `rsclaw::provider::`, etc.
//!
//! The binary entry-point is in `main.rs`; the composition root + dispatch
//! logic lives in `rsclaw-runtime`.
#![recursion_limit = "256"]
#![allow(unused_imports)]

// Composition root + RPC handlers (a2a, cmd, cron runner, gateway, hooks,
// server, ws) plus the `run` entry point.
pub use rsclaw_runtime::*;

// Re-export the 7 root-knot modules explicitly so `rsclaw::a2a::`,
// `rsclaw::ws::`, etc. resolve as module paths (glob import alone does not
// re-export child modules as paths).
pub use rsclaw_runtime::{a2a, cmd, cron, gateway, hooks, server, ws};

// Extracted base crates — re-exported under their historical module paths so
// integration tests using `rsclaw::config::` / `rsclaw::provider::` etc. keep
// resolving.
pub use rsclaw_agent as agent;
pub use rsclaw_artifact as artifact;
pub use rsclaw_browser as browser;
pub use rsclaw_cap as cap;
pub use rsclaw_channel as channel;
pub use rsclaw_cli as cli;
pub use rsclaw_computer as computer;
pub use rsclaw_config as config;
pub use rsclaw_desktop as desktop;
pub use rsclaw_embed as embed;
pub use rsclaw_events as events;
pub use rsclaw_heartbeat as heartbeat;
pub use rsclaw_i18n as i18n;
pub use rsclaw_kb as kb;
pub use rsclaw_mcp as mcp;
pub use rsclaw_migrate as migrate;
pub use rsclaw_platform as sys;
pub use rsclaw_plugin as plugin;
pub use rsclaw_provider as provider;
pub use rsclaw_skill as skill;
pub use rsclaw_store as store;
pub use rsclaw_util as util;

pub use rsclaw_platform::MemoryTier;

/// Ensure the rustls TLS crypto provider is installed for all lib tests.
/// This runs once before any test in the crate, preventing "No provider set"
/// panics when tests that construct `reqwest::Client` run in parallel.
#[cfg(test)]
#[ctor::ctor]
fn init_test_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
