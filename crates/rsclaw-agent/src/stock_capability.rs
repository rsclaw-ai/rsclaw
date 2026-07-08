//! astock-core capability adapters for rsclaw.
//!
//! Bridges rsclaw's built-in implementations (BrowserPool, config)
//! to astock-core's trait interfaces, enabling the stock tools to work.

use std::sync::Arc;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use astock_core::capability::{CdpCapability, TabHandle, ConfigProvider};

// ============================================================================
// CDP Adapter - bridges rsclaw-browser::BrowserPool to CdpCapability
// ============================================================================

/// Adapter for rsclaw's BrowserPool to astock-core's CdpCapability.
pub struct RsclawCdp;

impl RsclawCdp {
    pub fn new() -> Self {
        Self
    }
}

impl std::fmt::Debug for RsclawCdp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RsclawCdp")
    }
}

#[async_trait]
impl CdpCapability for RsclawCdp {
    async fn acquire_tab(&self) -> Result<Box<dyn TabHandle + Send>> {
        let pool = rsclaw_browser::pool::BrowserPool::global();
        let tab = pool.acquire_tab().await?;
        Ok(Box::new(RsclawTab(tab)))
    }
}

/// Wrapper for rsclaw's TabSession to implement TabHandle.
pub struct RsclawTab(rsclaw_browser::pool::TabSession);

#[async_trait]
impl TabHandle for RsclawTab {
    async fn navigate(&self, url: &str) -> Result<()> {
        self.0.navigate(url).await
    }

    async fn evaluate(&self, js: &str) -> Result<Value> {
        self.0.evaluate(js).await
    }

    async fn content(&self) -> Result<String> {
        self.0.get_html().await
    }

    async fn wait_for_selector(&self, selector: &str, timeout_ms: u64) -> Result<()> {
        self.0.wait_for_selector(selector, timeout_ms / 1000).await
    }

    async fn get_url(&self) -> Result<String> {
        let url = self.0.evaluate("window.location.href").await?;
        Ok(url.as_str().unwrap_or("").to_owned())
    }
}

// ============================================================================
// Config Adapter - reads from rsclaw.json5
// ============================================================================

/// Load rsclaw.json5 as raw JSON value (for extracting custom fields like astock).
fn load_raw_config() -> Option<Value> {
    use rsclaw_config::loader::base_dir;
    use std::fs;

    let config_path = base_dir().join("rsclaw.json5");
    let raw = fs::read_to_string(&config_path).ok()?;
    json5::from_str(&raw).ok()
}

/// Adapter for rsclaw's config to astock-core's ConfigProvider.
///
/// Reads stock-related settings from rsclaw.json5:
/// - `astock.tushare_token` or `tushare_token`
/// - `astock.*` other settings
pub struct RsclawConfig {
    /// Cached tushare token
    tushare_token: Option<String>,
}

impl RsclawConfig {
    pub fn new() -> Self {
        Self::load_from_config()
    }

    fn load_from_config() -> Self {
        let cfg = load_raw_config();

        // Try astock.tushare_token first, then tushare_token
        let tushare_token = cfg
            .as_ref()
            .and_then(|c| c.get("astock"))
            .and_then(|a| a.get("tushare_token"))
            .and_then(|t| t.as_str())
            .map(String::from)
            .or_else(|| {
                cfg.as_ref()
                    .and_then(|c| c.get("tushare_token"))
                    .and_then(|t| t.as_str())
                    .map(String::from)
            });

        Self { tushare_token }
    }
}

impl std::fmt::Debug for RsclawConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsclawConfig")
            .field("tushare_token", &self.tushare_token.as_ref().map(|_| "set"))
            .finish()
    }
}

impl ConfigProvider for RsclawConfig {
    fn tushare_token(&self) -> Option<String> {
        self.tushare_token.clone()
    }

    fn get(&self, key: &str) -> Option<String> {
        let cfg = load_raw_config();

        cfg.as_ref()
            .and_then(|c| c.get("astock"))
            .and_then(|a| a.get(key))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                cfg.as_ref()
                    .and_then(|c| c.get(key))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
    }
}

// ============================================================================
// HTTP Adapter - create DefaultHttp directly
// ============================================================================

/// Create HTTP client for stock engine.
pub fn create_http_client() -> Result<astock_core::capability::DefaultHttp> {
    astock_core::capability::DefaultHttp::new()
}