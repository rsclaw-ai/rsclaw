//! Shared browser pool — manages a headless Chrome with concurrent tabs.
//!
//! Instead of each agent launching its own Chrome process, all agents share
//! a pool of tabs within one (or a few) headless Chrome instances. This
//! dramatically reduces memory usage when multiple task agents run in parallel.
//!
//! Usage:
//!   let pool = BrowserPool::global();
//!   let tab = pool.acquire_tab().await?;
//!   tab.navigate("https://example.com").await?;
//!   let html = tab.get_text().await?;
//!   drop(tab); // tab is closed automatically

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, info, warn};

use super::{CdpClient, ChromeProcess, can_launch_chrome, ACTIVE_INSTANCES};
use crate::agent::platform::detect_chrome;

/// Maximum concurrent tabs per Chrome instance.
const MAX_TABS_PER_INSTANCE: usize = 8;

/// Idle timeout for the shared Chrome instance (longer than per-agent).
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600); // 10 minutes

/// A shared headless Chrome pool.
///
/// Manages one headless Chrome process with multiple concurrent tabs.
/// Each tab has its own CDP connection and can operate independently.
pub struct BrowserPool {
    /// The shared Chrome process (lazy-initialized).
    chrome: Mutex<Option<PooledChrome>>,
    /// Semaphore to limit concurrent tabs.
    tab_semaphore: Arc<Semaphore>,
    /// Chrome binary path (resolved once).
    chrome_path: Mutex<Option<String>>,
    /// Chrome profile name (shares cookies with headed browser).
    profile: Mutex<Option<String>>,
    /// Last activity timestamp (for idle reaping).
    last_activity: AtomicU64,
    /// Counter for round-robin engine selection.
    engine_counter: std::sync::atomic::AtomicU32,
}

/// Internal: a Chrome process with its debug port.
struct PooledChrome {
    process: ChromeProcess,
    port: u16,
}

/// A leased tab from the pool.
///
/// Holds an independent CDP connection to a tab within the shared Chrome.
/// The tab is automatically closed when this handle is dropped.
pub struct TabSession {
    cdp: CdpClient,
    target_id: String,
    port: u16,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl BrowserPool {
    /// Create a new pool (does not launch Chrome yet).
    pub fn new() -> Self {
        Self {
            chrome: Mutex::new(None),
            tab_semaphore: Arc::new(Semaphore::new(MAX_TABS_PER_INSTANCE)),
            chrome_path: Mutex::new(None),
            profile: Mutex::new(None),
            last_activity: AtomicU64::new(now_ms()),
            engine_counter: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Get or create the global shared pool.
    pub fn global() -> &'static BrowserPool {
        static POOL: std::sync::OnceLock<BrowserPool> = std::sync::OnceLock::new();
        POOL.get_or_init(BrowserPool::new)
    }

    /// Acquire a tab from the pool.
    ///
    /// Creates a new tab in the shared headless Chrome instance.
    /// Returns a `TabSession` with an independent CDP connection.
    /// The tab is closed when the `TabSession` is dropped.
    pub async fn acquire_tab(&self) -> Result<TabSession> {
        // Acquire semaphore permit (limits concurrent tabs).
        let permit = self.tab_semaphore.clone().acquire_owned().await
            .map_err(|_| anyhow!("browser pool semaphore closed"))?;

        // Whole acquire_tab has a 15s timeout — if Chrome is unresponsive,
        // bail out instead of blocking the caller indefinitely.
        match tokio::time::timeout(Duration::from_secs(30), self.acquire_tab_inner(permit)).await {
            Ok(result) => result,
            Err(_) => {
                warn!("pool: acquire_tab timed out (30s), Chrome may be unresponsive");
                Err(anyhow!("browser pool: timed out connecting to Chrome"))
            }
        }
    }

    /// Inner logic for acquire_tab, wrapped by a timeout.
    async fn acquire_tab_inner(&self, permit: tokio::sync::OwnedSemaphorePermit) -> Result<TabSession> {
        // Ensure Chrome is running.
        let port = self.ensure_chrome().await?;

        // Create a new tab.
        let discovery_url = format!("http://127.0.0.1:{port}/json");

        // Use the browser-level CDP to create a new target.
        let browser_ws = format!("http://127.0.0.1:{port}/json/version");
        let version_info: Value = reqwest::get(&browser_ws).await?.json().await?;
        let browser_ws_url = version_info["webSocketDebuggerUrl"]
            .as_str()
            .ok_or_else(|| anyhow!("pool: no browser webSocketDebuggerUrl"))?;

        let browser_cdp = CdpClient::connect(browser_ws_url).await?;
        let create_result = browser_cdp.send("Target.createTarget", json!({
            "url": "about:blank"
        })).await?;
        let target_id = create_result["targetId"]
            .as_str()
            .ok_or_else(|| anyhow!("pool: Target.createTarget did not return targetId"))?
            .to_owned();

        // Discover the new tab's WebSocket URL.
        let targets: Vec<Value> = reqwest::get(&discovery_url).await?.json().await?;
        let tab_ws_url = targets.iter()
            .find(|t| t["id"].as_str() == Some(&target_id))
            .and_then(|t| t["webSocketDebuggerUrl"].as_str())
            .ok_or_else(|| anyhow!("pool: new tab {target_id} not found in target list"))?
            .to_owned();

        // Connect CDP to the new tab.
        let cdp = CdpClient::connect(&tab_ws_url).await?;
        cdp.send("Page.enable", json!({})).await?;
        cdp.send("DOM.enable", json!({})).await?;
        cdp.send("Runtime.enable", json!({})).await?;
        cdp.send("Network.enable", json!({})).await?;

        self.touch();

        debug!(target_id = %target_id, "pool: tab acquired");

        Ok(TabSession {
            cdp,
            target_id,
            port,
            _permit: permit,
        })
    }

    /// Return a browser-level WebSocket URL into the pool's shared Chrome.
    /// Launches the pool Chrome if it isn't running. This lets
    /// `BrowserSession::connect_existing` reuse the same Chrome process —
    /// so sub-agents get a new tab instead of launching yet another Chrome.
    pub async fn chrome_ws_url(&self) -> Result<String> {
        let port = self.ensure_chrome().await?;
        let version_info: Value =
            reqwest::get(format!("http://127.0.0.1:{port}/json/version"))
                .await?
                .json()
                .await?;
        version_info["webSocketDebuggerUrl"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("pool: /json/version missing webSocketDebuggerUrl"))
    }

    /// Ensure the shared headless Chrome is running. Returns the debug port.
    async fn ensure_chrome(&self) -> Result<u16> {
        let mut guard = self.chrome.lock().await;

        // Check if existing process is still alive.
        if let Some(ref mut pooled) = *guard {
            if pooled.process.child.try_wait().is_ok_and(|s| s.is_some()) {
                warn!("pool: Chrome process exited, will restart");
                ACTIVE_INSTANCES.fetch_sub(1, Ordering::Relaxed);
                *guard = None;
            } else {
                return Ok(pooled.port);
            }
        }

        // Resolve chrome path (cached).
        let chrome_path = {
            let mut path_guard = self.chrome_path.lock().await;
            if path_guard.is_none() {
                *path_guard = detect_chrome();
            }
            path_guard.clone()
                .ok_or_else(|| anyhow!("pool: Chrome not found"))?
        };

        can_launch_chrome()?;

        // Resolve profile (shares cookies with the headed browser).
        let profile = {
            let mut profile_guard = self.profile.lock().await;
            if profile_guard.is_none() {
                let config_path = crate::config::loader::base_dir().join("rsclaw.json5");
                let cfg_profile = crate::config::loader::load_json5(&config_path)
                    .ok()
                    .and_then(|c| c.tools)
                    .and_then(|t| t.web_browser)
                    .and_then(|b| b.profile);
                *profile_guard = cfg_profile;
            }
            profile_guard.clone()
        };

        // Launch headless Chrome with shared profile (for cookies/session).
        let process = ChromeProcess::launch(&chrome_path, false, profile.as_deref()).await?;
        let port = process.port();
        info!(port, profile = ?profile, "pool: shared headless Chrome launched");

        *guard = Some(PooledChrome { process, port });
        Ok(port)
    }

    /// Get the next engine index for round-robin engine selection.
    /// This ensures concurrent searches use different engines to avoid CAPTCHA.
    pub fn next_engine_index(&self) -> u32 {
        self.engine_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Update last activity timestamp.
    fn touch(&self) {
        self.last_activity.store(now_ms(), Ordering::Relaxed);
    }

    /// Check if the pool has been idle too long.
    pub fn is_idle_expired(&self) -> bool {
        let last = self.last_activity.load(Ordering::Relaxed);
        let elapsed = now_ms().saturating_sub(last);
        elapsed > POOL_IDLE_TIMEOUT.as_millis() as u64
    }

    /// Shut down the shared Chrome if idle.
    pub async fn reap_if_idle(&self) {
        if !self.is_idle_expired() {
            return;
        }
        let mut guard = self.chrome.lock().await;
        if guard.is_some() {
            info!("pool: idle timeout, shutting down shared Chrome");
            *guard = None; // ChromeProcess::Drop kills the process
        }
    }
}

impl TabSession {
    /// Navigate the tab to a URL and wait for load.
    pub async fn navigate(&self, url: &str) -> Result<()> {
        self.cdp.send("Page.navigate", json!({"url": url})).await?;
        // Wait for load event with timeout.
        let _ = tokio::time::timeout(
            Duration::from_secs(15),
            self.cdp.wait_event("Page.loadEventFired", 15),
        ).await;
        Ok(())
    }

    /// Wait for an element matching the CSS selector to appear.
    pub async fn wait_for_selector(&self, selector: &str, timeout_secs: u64) -> Result<()> {
        let js = format!(
            r#"new Promise((resolve, reject) => {{
                const check = () => {{
                    if (document.querySelector({sel})) return resolve(true);
                    setTimeout(check, 200);
                }};
                check();
                setTimeout(() => reject('timeout'), {ms});
            }})"#,
            sel = serde_json::to_string(selector)?,
            ms = timeout_secs * 1000,
        );
        let _ = tokio::time::timeout(
            Duration::from_secs(timeout_secs + 1),
            self.cdp.send("Runtime.evaluate", json!({
                "expression": js,
                "awaitPromise": true,
            })),
        ).await;
        Ok(())
    }

    /// Execute JavaScript and return the result.
    pub async fn evaluate(&self, js: &str) -> Result<Value> {
        let result = self.cdp.send("Runtime.evaluate", json!({
            "expression": js,
            "returnByValue": true,
        })).await?;
        Ok(result["result"]["value"].clone())
    }

    /// Get the full text content of the page.
    pub async fn get_text(&self) -> Result<String> {
        let result = self.evaluate("document.body?.innerText || ''").await?;
        Ok(result.as_str().unwrap_or("").to_owned())
    }

    /// Get page HTML.
    pub async fn get_html(&self) -> Result<String> {
        let result = self.evaluate("document.documentElement?.outerHTML || ''").await?;
        Ok(result.as_str().unwrap_or("").to_owned())
    }

}

impl Drop for TabSession {
    fn drop(&mut self) {
        let target_id = self.target_id.clone();
        let port = self.port;
        debug!(target_id = %target_id, "pool: releasing tab");
        // Spawn async cleanup (can't await in drop).
        // Guard against being called outside a tokio runtime (e.g. during shutdown).
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let browser_ws = format!("http://127.0.0.1:{port}/json/version");
            if let Ok(resp) = reqwest::get(&browser_ws).await {
                if let Ok(info) = resp.json::<Value>().await {
                    if let Some(ws_url) = info["webSocketDebuggerUrl"].as_str() {
                        if let Ok(browser_cdp) = CdpClient::connect(ws_url).await {
                            let _ = browser_cdp.send("Target.closeTarget", json!({
                                "targetId": target_id
                            })).await;
                        }
                    }
                }
            }
        });
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
