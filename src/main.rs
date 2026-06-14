#![allow(unused)]
//! rsclaw — AI Agent Engine Compatible with OpenClaw
//!
//! Thin binary shim. The composition root + dispatch logic lives in the
//! `rsclaw-runtime` crate (`rsclaw_runtime::run`). `main` only installs the TLS
//! crypto provider, detects the memory tier, builds the tokio runtime and hands
//! off to `run`.
//!
//! Architecture reference: AGENTS.md

use anyhow::Result;

fn main() -> Result<()> {
    // Install the TLS crypto provider (aws-lc-rs) before any HTTP client is
    // created.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    // Detect available memory before spinning up the runtime.
    let tier = rsclaw_platform::detect_memory_tier();

    // Build an appropriate tokio runtime for the detected hardware tier.
    let rt = rsclaw_platform::build_runtime(tier)?;
    rt.block_on(rsclaw_runtime::run())
}
