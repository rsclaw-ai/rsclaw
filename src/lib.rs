//! rsclaw library crate — exposes all modules for integration tests and
//! future embedding use-cases.  The binary entry-point is in `main.rs`.

pub mod a2a;
pub mod acp;
pub mod agent;
pub mod browser;
pub mod channel;
pub mod cli;
pub mod cmd;
pub mod config;
pub mod cron;
pub mod events;
pub mod gateway;
pub mod hooks;
pub mod i18n;
pub mod mcp;
pub mod migrate;
pub mod plugin;
pub mod provider;
pub mod server;
pub mod skill;
pub mod store;
pub mod sys;
pub mod ws;

pub use sys::MemoryTier;

/// Ensure the rustls TLS crypto provider is installed for all lib tests.
/// This runs once before any test in the crate, preventing "No provider set"
/// panics when tests that construct `reqwest::Client` run in parallel.
#[cfg(test)]
#[ctor::ctor]
fn init_test_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
