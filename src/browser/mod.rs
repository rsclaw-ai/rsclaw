//! CDP (Chrome DevTools Protocol) browser automation.
//!
//! Provides `BrowserSession` -- a high-level API that launches a headless
//! Chrome process, connects via WebSocket, and exposes actions such as
//! navigate, snapshot (accessibility-like tree), click, fill, screenshot, etc.

pub mod pool;

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{Mutex, mpsc, oneshot},
    time,
};
use tracing::{debug, info, warn};

/// Minimum available memory (bytes) required to launch a new Chrome instance.
const MIN_AVAILABLE_MEMORY: u64 = 200 * 1024 * 1024; // 200 MB

/// Idle timeout: kill Chrome after this long without any tool call.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

/// Global counter of active Chrome instances.
static ACTIVE_INSTANCES: AtomicU32 = AtomicU32::new(0);

/// Compute max Chrome instances based on available memory.
/// Rule: (available - 500MB reserve) / 1GB, minimum 1.
fn max_instances() -> u32 {
    let available = available_memory_bytes();
    let reserve = 500 * 1024 * 1024; // 500MB for system
    let usable = available.saturating_sub(reserve);
    ((usable / (1024 * 1024 * 1024)) as u32).max(1)
}

/// Get total system physical memory in bytes.
fn total_system_memory_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut size: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let name = c"hw.memsize";
        unsafe {
            libc::sysctl(
                name.as_ptr() as *mut _,
                2,
                &mut size as *mut _ as *mut _,
                &mut len,
                std::ptr::null_mut(),
                0,
            );
        }
        if size > 0 { size } else { 8 * 1024 * 1024 * 1024 } // fallback 8GB
    }
    #[cfg(target_os = "linux")]
    {
        let info = unsafe {
            let mut info: libc::sysinfo = std::mem::zeroed();
            libc::sysinfo(&mut info);
            info
        };
        info.totalram as u64 * info.mem_unit as u64
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        8 * 1024 * 1024 * 1024 // fallback 8GB
    }
}

/// Get available (free) system memory in bytes.
fn available_memory_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        // vm_stat based: free + inactive pages
        if let Ok(output) = std::process::Command::new("vm_stat").output() {
            let text = String::from_utf8_lossy(&output.stdout);
            let page_size: u64 = 16384; // Apple Silicon default
            let mut free: u64 = 0;
            let mut inactive: u64 = 0;
            for line in text.lines() {
                if line.starts_with("Pages free:") {
                    free = line.split(':').nth(1)
                        .map(|s| s.trim().trim_end_matches('.').parse().unwrap_or(0))
                        .unwrap_or(0);
                } else if line.starts_with("Pages inactive:") {
                    inactive = line.split(':').nth(1)
                        .map(|s| s.trim().trim_end_matches('.').parse().unwrap_or(0))
                        .unwrap_or(0);
                }
            }
            return (free + inactive) * page_size;
        }
        2 * 1024 * 1024 * 1024 // fallback 2GB
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            for line in meminfo.lines() {
                if line.starts_with("MemAvailable:") {
                    let kb: u64 = line.split_whitespace().nth(1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    return kb * 1024;
                }
            }
        }
        2 * 1024 * 1024 * 1024
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        2 * 1024 * 1024 * 1024
    }
}

/// Check if we can launch a new Chrome instance (memory + instance limit).
pub fn can_launch_chrome() -> Result<()> {
    let active = ACTIVE_INSTANCES.load(Ordering::Relaxed);
    let max = max_instances();
    if active >= max {
        bail!(
            "Chrome instance limit reached ({active}/{max}). \
             System has {} GB total memory. Close other browser sessions first.",
            total_system_memory_bytes() / (1024 * 1024 * 1024)
        );
    }

    let available = available_memory_bytes();
    if available < MIN_AVAILABLE_MEMORY {
        bail!(
            "Insufficient memory to launch Chrome. Available: {} MB, required: {} MB. \
             Please close other applications and retry.",
            available / (1024 * 1024),
            MIN_AVAILABLE_MEMORY / (1024 * 1024),
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// ChromeProcess -- launch and manage a headless Chrome instance
// ---------------------------------------------------------------------------

pub(crate) struct ChromeProcess {
    child: tokio::process::Child,
    ws_url: String,
    _tmp_dir: Option<tempfile::TempDir>,
}

impl ChromeProcess {
    pub(crate) async fn launch(chrome_path: &str, headed: bool, profile: Option<&str>) -> Result<Self> {
        can_launch_chrome()?;

        // Resolve user-data-dir: named profile or temp dir.
        let (user_data_dir, tmp_dir) = if let Some(profile_name) = profile {
            let profile_dir = if profile_name == "default" {
                // Use Chrome's default user data directory.
                #[cfg(target_os = "macos")]
                let dir = dirs_next::home_dir()
                    .unwrap_or_default()
                    .join("Library/Application Support/Google/Chrome");
                #[cfg(target_os = "windows")]
                let dir = dirs_next::data_local_dir()
                    .unwrap_or_default()
                    .join("Google/Chrome/User Data");
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                let dir = dirs_next::config_dir()
                    .unwrap_or_default()
                    .join("google-chrome");
                dir
            } else {
                // Named profile under ~/.rsclaw/browser-profiles/
                crate::config::loader::base_dir()
                    .join("browser-profiles")
                    .join(profile_name)
            };
            std::fs::create_dir_all(&profile_dir).ok();
            // Kill stale Chrome processes using this profile (e.g. after gateway restart).
            let profile_str = profile_dir.to_string_lossy().to_string();
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("pkill")
                    .args(["-9", "-f", &format!("user-data-dir={}", profile_str)])
                    .output();
                // Brief pause for processes to exit.
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            // Remove stale lock files from previous Chrome instances.
            for lock_file in &["SingletonLock", "SingletonSocket", "SingletonCookie"] {
                let p = profile_dir.join(lock_file);
                if p.exists() {
                    std::fs::remove_file(&p).ok();
                    info!(file = %p.display(), "removed stale Chrome lock file");
                }
            }
            (profile_dir, None)
        } else {
            let tmp = tempfile::tempdir()
                .map_err(|e| anyhow!("failed to create temp dir for Chrome profile: {e}"))?;
            let dir = tmp.path().to_path_buf();
            (dir, Some(tmp))
        };

        let mut args = vec![
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-backgrounding-occluded-windows",
            "--disable-component-update",
            "--disable-default-apps",
            "--disable-hang-monitor",
            "--disable-popup-blocking",
            "--disable-prompt-on-repost",
            "--disable-sync",
            "--disable-features=Translate",
            "--enable-features=NetworkService,NetworkServiceInProcess",
            "--metrics-recording-only",
            "--password-store=basic",
            "--use-mock-keychain",
            "--window-size=1280,720",
            "about:blank",
        ];
        if !headed {
            args.push("--headless=new");
        }

        let mut child = tokio::process::Command::new(chrome_path)
            .args(&args)
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("failed to launch Chrome at {chrome_path}: {e}"))?;

        // Read stderr until we find the DevTools WebSocket URL.
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("no stderr from Chrome process"))?;
        let mut reader = BufReader::new(stderr).lines();

        let ws_url = time::timeout(Duration::from_secs(30), async {
            while let Some(line) = reader.next_line().await? {
                debug!(line = %line, "chrome stderr");
                if let Some(pos) = line.find("ws://") {
                    return Ok::<String, anyhow::Error>(line[pos..].trim().to_owned());
                }
            }
            Err(anyhow!("Chrome exited without printing DevTools URL"))
        })
        .await
        .map_err(|e| {
            // Chrome launched but didn't give us a WebSocket URL — kill it.
            let _ = child.start_kill();
            anyhow!("timed out waiting for Chrome DevTools URL: {}", e)
        })??;

        debug!(ws_url = %ws_url, "Chrome DevTools URL discovered");
        ACTIVE_INSTANCES.fetch_add(1, Ordering::Relaxed);
        let active = ACTIVE_INSTANCES.load(Ordering::Relaxed);
        let max = max_instances();
        info!(active, max, "Chrome instance launched");

        Ok(Self {
            child,
            ws_url,
            _tmp_dir: tmp_dir,
        })
    }

    /// Extract the debugging port from the ws URL.
    pub(crate) fn port(&self) -> Result<u16> {
        parse_port_from_ws_url(&self.ws_url)
    }
}

impl Drop for ChromeProcess {
    fn drop(&mut self) {
        // Kill the Chrome process and wait for it to exit.
        // On Windows, start_kill() sends a termination signal but the process
        // may not exit immediately. Chrome also spawns sub-processes (renderer,
        // gpu, etc.) that need to be cleaned up. We use taskkill on Windows to
        // kill the entire process tree, and a longer poll loop on other platforms.

        #[cfg(target_os = "windows")]
        {
            if let Some(pid) = self.child.id() {
                // Use taskkill to kill the entire process tree on Windows.
                // /T = kill process and all child processes, /F = force kill.
                let _ = std::process::Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &pid.to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            let killed = self.child.start_kill().is_ok();
            if killed {
                // Poll for exit with a longer timeout to avoid leaving zombies
                let mut attempts = 0;
                while attempts < 100 {
                    match self.child.try_wait() {
                        Ok(Some(_)) => break,    // Process exited
                        Ok(None) => {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            attempts += 1;
                        }
                        Err(_) => break,          // Can't query, assume dead
                    }
                }
            }
        }

        ACTIVE_INSTANCES.fetch_sub(1, Ordering::Relaxed);
        debug!("Chrome instance dropped, active={}", ACTIVE_INSTANCES.load(Ordering::Relaxed));
    }
}

/// Parse the port number from a Chrome DevTools WebSocket URL.
/// Expects format: `ws://127.0.0.1:PORT/devtools/...`
fn parse_port_from_ws_url(url: &str) -> Result<u16> {
    let after_host = url
        .find("127.0.0.1:")
        .map(|i| i + "127.0.0.1:".len())
        .ok_or_else(|| anyhow!("cannot parse port from ws URL: {url}"))?;
    let end = url[after_host..]
        .find('/')
        .unwrap_or(url.len() - after_host);
    let port_str = &url[after_host..after_host + end];
    port_str
        .parse::<u16>()
        .map_err(|e| anyhow!("invalid port in ws URL: {e}"))
}

/// Try to connect to an already-running Chrome with remote debugging.
/// Probes the given ports and returns the browser WebSocket URL if found.
pub(crate) async fn detect_existing_chrome(ports: &[u16]) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .ok()?;

    for &port in ports {
        let url = format!("http://127.0.0.1:{port}/json/version");
        debug!(port, "probing for existing Chrome remote debugging");
        match client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(body) = resp.json::<Value>().await {
                    if let Some(ws_url) = body.get("webSocketDebuggerUrl").and_then(|v| v.as_str()) {
                        debug!(port, ws_url, "found existing Chrome with remote debugging");
                        return Some(ws_url.to_owned());
                    }
                }
            }
            Err(e) => {
                debug!(port, error = %e, "no Chrome remote debugging on this port");
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// CdpClient -- WebSocket CDP transport
// ---------------------------------------------------------------------------

pub(crate) struct CdpClient {
    ws_url: String,
    ws_tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Value>>>>,
    events_rx: Mutex<mpsc::UnboundedReceiver<Value>>,
    next_id: AtomicU32,
}

impl CdpClient {
    pub(crate) async fn connect(ws_url: &str) -> Result<Self> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| anyhow!("CDP WebSocket connect failed: {e}"))?;

        let (mut ws_sink, mut ws_source) = ws_stream.split();

        // Channel for outbound frames.
        let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<String>();

        // Channel for inbound events (non-response messages).
        let (events_tx, events_rx) = mpsc::unbounded_channel::<Value>();

        // Pending response waiters.
        let pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = Arc::clone(&pending);

        // Writer task.
        tokio::spawn(async move {
            while let Some(msg) = ws_rx.recv().await {
                use tokio_tungstenite::tungstenite::Message;
                if ws_sink.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
        });

        // Reader task.
        tokio::spawn(async move {
            while let Some(Ok(frame)) = ws_source.next().await {
                let text = match frame {
                    tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
                    _ => continue,
                };
                let Ok(val) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = val.get("id").and_then(|v| v.as_u64()) {
                    let mut map = pending_reader.lock().await;
                    if let Some(tx) = map.remove(&(id as u32)) {
                        let _ = tx.send(val);
                    }
                } else {
                    // It is an event.
                    let _ = events_tx.send(val);
                }
            }
        });

        Ok(Self {
            ws_url: ws_url.to_owned(),
            ws_tx,
            pending,
            events_rx: Mutex::new(events_rx),
            next_id: AtomicU32::new(1),
        })
    }

    fn ws_url(&self) -> &str {
        &self.ws_url
    }

    /// Send a CDP command and wait for the matching response.
    pub(crate) async fn send(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(id, tx);
        }

        self.ws_tx
            .send(msg.to_string())
            .map_err(|_| anyhow!("CDP WebSocket closed"))?;

        let resp = time::timeout(Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow!("CDP response timeout for {method}"))?
            .map_err(|_| anyhow!("CDP response channel closed for {method}"))?;

        if let Some(err) = resp.get("error") {
            bail!("CDP error for {method}: {err}");
        }

        Ok(resp.get("result").cloned().unwrap_or(json!({})))
    }

    /// Wait for a specific event, with timeout.
    pub(crate) async fn wait_event(&self, event_method: &str, timeout_secs: u64) -> Result<Value> {
        let deadline = time::Instant::now() + Duration::from_secs(timeout_secs);
        let mut rx = self.events_rx.lock().await;
        loop {
            match time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(val)) => {
                    if val.get("method").and_then(|m| m.as_str()) == Some(event_method) {
                        return Ok(val);
                    }
                    // Not the event we want, keep waiting.
                }
                Ok(None) => bail!("CDP event stream closed while waiting for {event_method}"),
                Err(_) => bail!("timeout waiting for CDP event {event_method}"),
            }
        }
    }

    /// Drain all pending events.
    fn drain_events(events_rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(val) = events_rx.try_recv() {
            out.push(val);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// BrowserSession -- high-level API
// ---------------------------------------------------------------------------

/// A live browser session backed by a headless Chrome process and CDP.
pub struct BrowserSession {
    /// Chrome process handle (killed on drop). None when connected to external Chrome.
    chrome: Option<ChromeProcess>,
    /// Remote debugging port extracted from the browser WS URL.
    /// Used for CDP discovery when `chrome` is None (external Chrome).
    debug_port: u16,
    cdp: CdpClient,
    /// @eN -> data-ref string mapping (kept in sync with snapshot).
    refs: HashMap<String, String>,
    /// Counter for next ref ID.
    ref_counter: u32,
    /// Chrome binary path (for restart).
    chrome_path: String,
    /// Run with visible window.
    pub headed: bool,
    /// Chrome profile name (for restart).
    profile: Option<String>,
    /// Pending dialog message (confirm/prompt).
    pending_dialog: Option<String>,
    /// Accumulated blocked URL patterns for network interception.
    blocked_urls: Vec<String>,
    /// Request interception rules: (url_pattern, action).
    intercept_rules: Vec<(String, String)>,
    /// Last activity timestamp (for idle timeout).
    last_activity: Arc<AtomicU64>,
    /// Stored screenshot for diff comparison.
    before_screenshot: Option<String>,
    /// Operation recording entries.
    recording: Option<Vec<Value>>,
}

impl BrowserSession {
    /// Return the Chrome remote debugging port.
    pub fn debug_port(&self) -> u16 {
        self.debug_port
    }

    /// Launch Chrome, discover the default page target, and connect CDP.
    pub async fn start(chrome_path: &str, headed: bool, profile: Option<&str>) -> Result<Self> {
        let chrome = ChromeProcess::launch(chrome_path, headed, profile).await?;
        let port = chrome.port()?;
        let cdp = Self::connect_cdp_by_port(port).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Ok(Self {
            chrome: Some(chrome),
            debug_port: port,
            cdp,
            refs: HashMap::new(),
            ref_counter: 0,
            chrome_path: chrome_path.to_owned(),
            headed,
            profile: profile.map(str::to_owned),
            pending_dialog: None,
            blocked_urls: Vec::new(),
            intercept_rules: Vec::new(),
            last_activity: Arc::new(AtomicU64::new(now)),
            before_screenshot: None,
            recording: None,
        })
    }

    /// Connect to an existing Chrome instance (user's daily browser).
    /// Does NOT launch or own a Chrome process -- will not kill it on drop.
    pub(crate) async fn connect_existing(browser_ws_url: &str) -> Result<Self> {
        Self::connect_existing_inner(browser_ws_url, false).await
    }

    /// Connect to existing Chrome, reusing the active tab instead of creating a new one.
    pub(crate) async fn connect_existing_reuse(browser_ws_url: &str) -> Result<Self> {
        Self::connect_existing_inner(browser_ws_url, true).await
    }

    async fn connect_existing_inner(browser_ws_url: &str, reuse_tab: bool) -> Result<Self> {
        // Extract port from browser WS URL (ws://127.0.0.1:PORT/devtools/browser/...)
        let port = parse_port_from_ws_url(browser_ws_url)?;

        let discovery_url = format!("http://127.0.0.1:{port}/json");
        let targets: Vec<Value> = reqwest::get(&discovery_url).await?.json().await?;

        let tab_ws_url = if reuse_tab {
            // Reuse the first existing page tab.
            targets.iter()
                .find(|t| t["type"].as_str() == Some("page"))
                .and_then(|t| t["webSocketDebuggerUrl"].as_str())
                .map(|s| s.to_owned())
        } else {
            None
        };

        let tab_ws_url = if let Some(url) = tab_ws_url {
            debug!("reusing existing tab");
            url
        } else {
            // Create a new tab.
            let browser_cdp = CdpClient::connect(browser_ws_url).await?;
            let result = browser_cdp.send("Target.createTarget", json!({"url": "about:blank"})).await?;
            let target_id = result.get("targetId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Target.createTarget did not return targetId"))?;
            debug!(target_id, "created new tab in external Chrome");

            // Re-fetch targets to find the new tab.
            let targets: Vec<Value> = reqwest::get(&discovery_url).await?.json().await?;
            targets.iter()
                .find(|t| t["id"].as_str() == Some(target_id))
                .and_then(|t| t["webSocketDebuggerUrl"].as_str())
                .ok_or_else(|| anyhow!("new tab not found in target list"))?
                .to_owned()
        };

        // Connect CDP to the tab.
        let cdp = CdpClient::connect(&tab_ws_url).await?;
        cdp.send("Page.enable", json!({})).await?;
        cdp.send("DOM.enable", json!({})).await?;
        cdp.send("Runtime.enable", json!({})).await?;
        cdp.send("Network.enable", json!({})).await?;
        cdp.send("Target.setDiscoverTargets", json!({"discover": true})).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Ok(Self {
            chrome: None,
            debug_port: port,
            cdp,
            refs: HashMap::new(),
            ref_counter: 0,
            chrome_path: String::new(),
            headed: true,
            profile: None,
            pending_dialog: None,
            blocked_urls: Vec::new(),
            intercept_rules: Vec::new(),
            last_activity: Arc::new(AtomicU64::new(now)),
            before_screenshot: None,
            recording: None,
        })
    }

    /// Connect CDP to a Chrome process's page target by port.
    /// Retries discovery up to 20 times (headed mode can be slow to initialize).
    async fn connect_cdp_by_port(port: u16) -> Result<CdpClient> {
        let discovery_url = format!("http://127.0.0.1:{port}/json");

        let mut page_target: Option<Value> = None;
        for attempt in 0..20 {
            if let Ok(resp) = reqwest::get(&discovery_url).await {
                if let Ok(targets) = resp.json::<Vec<Value>>().await {
                    if attempt == 0 || attempt == 9 || attempt == 19 {
                        let types: Vec<&str> = targets.iter()
                            .filter_map(|t| t["type"].as_str())
                            .collect();
                        debug!(attempt, ?types, "CDP target types discovered");
                    }
                    if let Some(target) = targets.into_iter().find(|t| t["type"].as_str() == Some("page")) {
                        page_target = Some(target);
                        break;
                    }
                }
            }
            if attempt < 19 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        let page_target = page_target
            .ok_or_else(|| anyhow!("no page target found in CDP target list after 20 attempts"))?;

        let page_ws_url = page_target["webSocketDebuggerUrl"]
            .as_str()
            .ok_or_else(|| anyhow!("page target missing webSocketDebuggerUrl"))?;

        debug!(page_ws_url = %page_ws_url, "connecting to page target");

        let cdp = CdpClient::connect(page_ws_url).await?;
        cdp.send("Page.enable", json!({})).await?;
        cdp.send("DOM.enable", json!({})).await?;
        cdp.send("Runtime.enable", json!({})).await?;
        cdp.send("Network.enable", json!({})).await?;
        cdp.send("Target.setDiscoverTargets", json!({"discover": true})).await?;

        Ok(cdp)
    }

    /// Check if Chrome is still alive. Returns false if the process exited.
    /// For external Chrome (chrome is None), always returns true (we do not own it).
    pub fn is_alive(&mut self) -> bool {
        match self.chrome {
            Some(ref mut chrome) => matches!(chrome.child.try_wait(), Ok(None)),
            None => true,
        }
    }

    /// Check idle timeout: returns true if session has been idle too long.
    pub fn is_idle_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = self.last_activity.load(Ordering::Relaxed);
        now.saturating_sub(last) > IDLE_TIMEOUT.as_secs()
    }

    /// Restart the Chrome process (e.g. after crash or idle expiry).
    /// For external Chrome (chrome is None), only reconnects CDP.
    async fn restart(&mut self) -> Result<()> {
        warn!("restarting Chrome browser session");
        if self.chrome.is_some() {
            // Drop old chrome (kills process via Drop) and launch new one.
            let new_chrome = ChromeProcess::launch(&self.chrome_path, self.headed, self.profile.as_deref()).await?;
            let port = new_chrome.port()?;
            let new_cdp = Self::connect_cdp_by_port(port).await?;
            self.debug_port = port;
            self.chrome = Some(new_chrome);
            self.cdp = new_cdp;
        } else {
            // External Chrome: create a fresh tab (don't hijack user's existing tabs).
            let browser_ws = format!(
                "ws://127.0.0.1:{}/devtools/browser",
                self.debug_port
            );
            // Try to get the full browser ws URL from /json/version.
            let version_url = format!("http://127.0.0.1:{}/json/version", self.debug_port);
            let browser_ws_url = match reqwest::Client::new()
                .get(&version_url)
                .timeout(Duration::from_secs(3))
                .send()
                .await
            {
                Ok(resp) => resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["webSocketDebuggerUrl"].as_str().map(String::from))
                    .unwrap_or(browser_ws),
                Err(_) => browser_ws,
            };
            let browser_cdp = CdpClient::connect(&browser_ws_url).await?;
            let create = browser_cdp.send("Target.createTarget", json!({"url": "about:blank"})).await?;
            let target_id = create["targetId"]
                .as_str()
                .ok_or_else(|| anyhow!("restart: no targetId from Target.createTarget"))?;
            // browser_cdp intentionally dropped here — we only needed it for Target.createTarget.
            drop(browser_cdp);

            let discovery = format!("http://127.0.0.1:{}/json", self.debug_port);
            let targets: Vec<serde_json::Value> = reqwest::get(&discovery).await?.json().await?;
            let tab_ws = targets
                .iter()
                .find(|t| t["id"].as_str() == Some(target_id))
                .and_then(|t| t["webSocketDebuggerUrl"].as_str())
                .ok_or_else(|| anyhow!("restart: new tab ws URL not found"))?;
            let new_cdp = CdpClient::connect(tab_ws).await?;
            new_cdp.send("Page.enable", json!({})).await?;
            new_cdp.send("DOM.enable", json!({})).await?;
            new_cdp.send("Runtime.enable", json!({})).await?;
            new_cdp.send("Network.enable", json!({})).await?;
            self.cdp = new_cdp;
        }
        self.refs.clear();
        self.ref_counter = 0;
        Ok(())
    }

    fn touch_activity(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.last_activity.store(now, Ordering::Relaxed);
    }

    /// Main dispatch: execute a browser action.
    pub async fn execute(&mut self, action: &str, args: &Value) -> Result<Value> {
        // Check liveness: if Chrome died, restart
        if !self.is_alive() {
            warn!("Chrome process not alive, restarting");
            self.restart().await?;
        }

        // Check idle timeout: if expired, restart for fresh state
        if self.is_idle_expired() {
            info!("Chrome idle timeout expired, restarting");
            self.restart().await?;
        }

        self.touch_activity();

        // Drain events, then handle dialogs and fetch interceptions outside the lock.
        let (dialog_events, fetch_events): (Vec<Value>, Vec<Value>) = {
            let mut rx = self.cdp.events_rx.lock().await;
            let mut dialogs = Vec::new();
            let mut fetches = Vec::new();
            while let Ok(event) = rx.try_recv() {
                match event.get("method").and_then(|m| m.as_str()) {
                    Some("Page.javascriptDialogOpening") => dialogs.push(event),
                    Some("Fetch.requestPaused") => fetches.push(event),
                    _ => {}
                }
            }
            (dialogs, fetches)
        };
        for event in &dialog_events {
            let msg = event["params"]["message"].as_str().unwrap_or("").to_string();
            let dtype = event["params"]["type"].as_str().unwrap_or("");
            if dtype == "alert" || dtype == "beforeunload" {
                let _ = self.cdp.send("Page.handleJavaScriptDialog", json!({"accept": true})).await;
            } else {
                self.pending_dialog = Some(msg);
            }
        }
        // Handle intercepted fetch requests.
        for event in &fetch_events {
            let req_id = event["params"]["requestId"].as_str().unwrap_or("");
            let req_url = event["params"]["request"]["url"].as_str().unwrap_or("");
            let mut handled = false;
            for (pattern, rule_action) in &self.intercept_rules {
                if req_url.contains(pattern.as_str()) {
                    if rule_action == "block" {
                        let _ = self.cdp.send("Fetch.failRequest", json!({
                            "requestId": req_id, "errorReason": "BlockedByClient"
                        })).await;
                    } else if let Some(body) = rule_action.strip_prefix("mock:") {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(body);
                        let _ = self.cdp.send("Fetch.fulfillRequest", json!({
                            "requestId": req_id,
                            "responseCode": 200,
                            "responseHeaders": [{"name": "Content-Type", "value": "application/json"}],
                            "body": encoded,
                        })).await;
                    }
                    handled = true;
                    break;
                }
            }
            if !handled {
                let _ = self.cdp.send("Fetch.continueRequest", json!({"requestId": req_id})).await;
            }
        }

        let result = match action {
            "open" | "navigate" => self.cmd_open(args).await,
            "snapshot" => self.cmd_snapshot(args).await,
            "click" => self.cmd_click(args).await,
            "clickAt" | "click_at" => self.cmd_click_at(args).await,
            "fill" | "type" => self.cmd_fill(args).await,
            "select" => self.cmd_select(args).await,
            "check" => self.cmd_check(args, true).await,
            "uncheck" => self.cmd_check(args, false).await,
            "hover" => self.cmd_hover(args).await,
            "dblclick" | "double_click" => self.cmd_dblclick(args).await,
            "drag" => self.cmd_drag(args).await,
            "focus" => self.cmd_focus(args).await,
            "scrollintoview" | "scroll_into_view" => self.cmd_scroll_into_view(args).await,
            "scroll" => self.cmd_scroll(args).await,
            "screenshot" => self.cmd_screenshot(args).await,
            "pdf" => self.cmd_pdf().await,
            "back" => self.cmd_back().await,
            "forward" => self.cmd_forward().await,
            "reload" => self.cmd_reload().await,
            "get_text" => self.cmd_get_text().await,
            "get_url" => self.cmd_get_url().await,
            "get_title" => self.cmd_get_title().await,
            "wait" => self.cmd_wait(args).await,
            "evaluate" => self.cmd_evaluate(args).await,
            "cookies" => self.cmd_cookies(args).await,
            "press" => self.cmd_press(args).await,
            "set_viewport" => self.cmd_set_viewport(args).await,
            "dialog" => self.cmd_dialog(args).await,
            "new_tab" => self.cmd_new_tab(args).await,
            "list_tabs" => self.cmd_list_tabs().await,
            "switch_tab" => self.cmd_switch_tab(args).await,
            "close_tab" => self.cmd_close_tab(args).await,
            "state" => self.cmd_state(args).await,
            "network" => self.cmd_network(args).await,
            "highlight" => self.cmd_highlight(args).await,
            "clipboard" => self.cmd_clipboard(args).await,
            "find" => self.cmd_find(args).await,
            "get_article" => self.cmd_get_article().await,
            "upload" => self.cmd_upload(args).await,
            "context" => self.cmd_context(args).await,
            "emulate" => self.cmd_emulate(args).await,
            "diff" => self.cmd_diff(args).await,
            "record" => self.cmd_record(args).await,
            "keydown" | "key_down" => self.cmd_keydown(args).await,
            "keyup" | "key_up" => self.cmd_keyup(args).await,
            "mouse" => self.cmd_mouse(args).await,
            "storage" => self.cmd_storage(args).await,
            "download_wait" => self.cmd_download_wait(args).await,
            "is" => self.cmd_is(args).await,
            "get" => self.cmd_get(args).await,
            "search" => self.cmd_search(args).await,
            "console" => self.cmd_console(args).await,
            "content" => self.cmd_content().await,
            "frame" => self.cmd_frame(args).await,
            "mainframe" | "main_frame" => self.cmd_mainframe().await,
            "waitforurl" | "wait_for_url" => self.cmd_wait_for_url(args).await,
            "getbytext" | "get_by_text" => self.cmd_getby(args, "text").await,
            "getbyrole" | "get_by_role" => self.cmd_getby(args, "role").await,
            "getbylabel" | "get_by_label" => self.cmd_getby(args, "label").await,
            other => Err(anyhow!("web_browser: unsupported action `{other}`")),
        };

        // Record operation if recording is active.
        if let Ok(ref _val) = result {
            if let Some(ref mut entries) = self.recording {
                if entries.len() < 200 {
                    let mut entry = json!({
                        "action": action,
                        "args": args,
                        "ts": chrono::Utc::now().timestamp_millis(),
                    });
                    // Capture low-quality screenshot for trace.
                    if let Ok(ss) = self.cdp.send("Page.captureScreenshot", json!({"format": "jpeg", "quality": 30})).await {
                        if let Some(data) = ss.get("data").and_then(|v| v.as_str()) {
                            entry["screenshot"] = json!(format!("data:image/jpeg;base64,{data}"));
                        }
                    }
                    entries.push(entry);
                }
            }
        }

        // If the command failed due to a CDP transport error (WebSocket disconnected,
        // Chrome crashed, etc.), restart the session. Do NOT restart for normal business
        // errors like "element not found" or "timeout".
        if let Err(ref e) = result {
            let msg = e.to_string();
            let is_transport_error = msg.contains("WebSocket")
                || msg.contains("connection")
                || msg.contains("broken pipe")
                || msg.contains("Connection reset")
                || msg.contains("EOF")
                || !self.is_alive();
            if is_transport_error {
                warn!("CDP transport error, restarting Chrome to recover");
                let _ = self.restart().await;
            }
        }

        result
    }

    // -- Command implementations ------------------------------------------------

    async fn cmd_open(&mut self, args: &Value) -> Result<Value> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("open: `url` required"))?;

        // Drain stale events before navigating.
        {
            let mut rx = self.cdp.events_rx.lock().await;
            CdpClient::drain_events(&mut rx);
        }

        self.cdp
            .send("Page.navigate", json!({ "url": url }))
            .await?;

        // Wait for page load.
        let _ = self.cdp.wait_event("Page.loadEventFired", 15).await;

        // Clear refs from previous page.
        self.refs.clear();
        self.ref_counter = 0;

        Ok(json!({ "action": "open", "url": url, "text": format!("Navigated to {url}") }))
    }

    async fn cmd_snapshot(&mut self, args: &Value) -> Result<Value> {
        // Clear old refs.
        self.refs.clear();
        self.ref_counter = 0;

        // interactive mode: only return elements with @ref (saves 80% tokens)
        let interactive = args.get("interactive").and_then(|v| v.as_bool()).unwrap_or(false)
            || args.get("i").and_then(|v| v.as_bool()).unwrap_or(false);
        let compact = args.get("compact").and_then(|v| v.as_bool()).unwrap_or(false);
        let max_depth = args.get("depth").and_then(|v| v.as_u64()).map(|d| d as usize);
        let selector = args.get("selector").and_then(|v| v.as_str()).filter(|s| !s.is_empty());

        let base_js = if interactive { SNAPSHOT_INTERACTIVE_JS } else { SNAPSHOT_JS };

        // If a CSS selector is specified, patch the JS to scope to that element.
        let js = if let Some(sel) = selector {
            let escaped = sel.replace('\\', "\\\\").replace('\'', "\\'");
            // Replace `walk(document.body, 0)` with scoped element lookup.
            base_js.replace(
                "if (document.body) walk(document.body, 0);",
                &format!(
                    "var __root = document.querySelector('{}'); if (__root) walk(__root, 0);",
                    escaped
                ),
            )
        } else {
            base_js.to_owned()
        };

        let result = self
            .cdp
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": js,
                    "returnByValue": true,
                }),
            )
            .await?;

        let raw = result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("{}");

        let parsed: Value =
            serde_json::from_str(raw).unwrap_or_else(|_| json!({"lines": [], "refCount": 0}));

        let line_strs: Vec<&str> = parsed
            .get("lines")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        // Post-processing: depth and compact filters.
        let lines: Vec<&str> = line_strs
            .into_iter()
            .filter(|line| {
                // Depth filter: count leading spaces, each level = 2 spaces.
                if let Some(max_d) = max_depth {
                    let indent = line.len() - line.trim_start().len();
                    if indent > max_d * 2 {
                        return false;
                    }
                }
                // Compact filter: remove lines that are only structural tags with no text.
                if compact {
                    let trimmed = line.trim();
                    // Structural-only lines look like [section], [div], [nav], etc.
                    // Keep lines that have text content, @ref markers, or meaningful info.
                    if trimmed.starts_with('[') && trimmed.ends_with(']') && !trimmed.contains('@') {
                        return false;
                    }
                }
                true
            })
            .collect();

        let text = lines.join("\n");

        let ref_count = parsed
            .get("refCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        // Rebuild refs map: @e1 .. @eN -> data-ref attribute values.
        for i in 1..=ref_count {
            let key = format!("@e{i}");
            self.refs.insert(key.clone(), key);
        }
        self.ref_counter = ref_count;

        Ok(json!({
            "action": "snapshot",
            "text": text,
        }))
    }

    async fn cmd_click(&self, args: &Value) -> Result<Value> {
        let eref = args.get("ref").and_then(|v| v.as_str());
        let text_sel = args.get("text").and_then(|v| v.as_str());

        let find_js = if let Some(eref) = eref {
            format!(
                r#"{FIND_REF_JS} var el=findRef('{}'); if(!el) return 'NOT_FOUND';"#,
                escape_js_string(eref),
                FIND_REF_JS = FIND_REF_JS,
            )
        } else if let Some(text) = text_sel {
            format!(
                r#"var el=(function(){{var t='{}';var all=document.querySelectorAll('a,button,[role=button],[role=link],input[type=submit]');for(var i=0;i<all.length;i++){{if(all[i].innerText&&all[i].innerText.trim().includes(t))return all[i];}};var everything=document.querySelectorAll('*');for(var i=0;i<everything.length;i++){{var s=window.getComputedStyle(everything[i]);if(s.cursor==='pointer'&&everything[i].innerText&&everything[i].innerText.trim()===t)return everything[i];}};for(var i=0;i<everything.length;i++){{var s=window.getComputedStyle(everything[i]);if(s.cursor==='pointer'&&everything[i].innerText&&everything[i].innerText.trim().includes(t))return everything[i];}};return null;}})(); if(!el) return 'NOT_FOUND';"#,
                escape_js_string(text),
            )
        } else {
            bail!("click: `ref` or `text` required");
        };

        let js = format!(
            r#"(async function(){{
                {find_js}
                {WAIT_ACTIONABLE_JS}
                var status = await waitActionable(el, 5000);
                if (status === 'TIMEOUT') return 'TIMEOUT';
                el.scrollIntoView({{block:'center'}});
                el.click();
                return 'OK';
            }})()"#,
            find_js = find_js,
            WAIT_ACTIONABLE_JS = WAIT_ACTIONABLE_JS,
        );

        let result = self.cdp.send("Runtime.evaluate", json!({
            "expression": js,
            "returnByValue": true,
            "awaitPromise": true,
        })).await?;
        let value = result.get("result").and_then(|r| r.get("value"))
            .and_then(|v| v.as_str()).unwrap_or("");

        match value {
            "NOT_FOUND" => bail!("click: element not found (ref={}, text={})",
                eref.unwrap_or(""), text_sel.unwrap_or("")),
            "TIMEOUT" => bail!("click: element not actionable within 5s"),
            _ => {}
        }

        Ok(json!({ "action": "click", "ref": eref, "text": format!("Clicked {}", eref.or(text_sel).unwrap_or("element")) }))
    }

    /// Dispatch a real mouse click via CDP `Input.dispatchMouseEvent`.
    ///
    /// Accepts either a `ref` (element reference from snapshot, e.g. "@e5")
    /// or explicit `x`/`y` pixel coordinates. When a ref is provided the
    /// element's bounding rect is queried and its center is used.
    ///
    /// Three low-level mouse events are sent with small delays to mimic a
    /// genuine user click: mouseMoved, mousePressed, mouseReleased.
    async fn cmd_click_at(&self, args: &Value) -> Result<Value> {
        let eref = args.get("ref").and_then(|v| v.as_str());
        let explicit_x = args.get("x").and_then(|v| v.as_f64());
        let explicit_y = args.get("y").and_then(|v| v.as_f64());

        let (x, y) = if let Some(eref) = eref {
            // Resolve element ref to center coordinates via getBoundingClientRect.
            let js = format!(
                r#"(function(){{
                    {FIND_REF_JS}
                    var el = findRef('{eref}');
                    if (!el) return JSON.stringify({{"error": "NOT_FOUND"}});
                    el.scrollIntoView({{block:'center'}});
                    var r = el.getBoundingClientRect();
                    return JSON.stringify({{
                        "x": Math.round(r.left + r.width / 2),
                        "y": Math.round(r.top + r.height / 2)
                    }});
                }})()"#,
                FIND_REF_JS = FIND_REF_JS,
                eref = escape_js_string(eref),
            );

            let result = self.cdp.send("Runtime.evaluate", json!({
                "expression": js,
                "returnByValue": true,
            })).await?;
            let raw = result
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("clickAt: failed to evaluate element position"))?;
            let parsed: Value = serde_json::from_str(raw)
                .map_err(|e| anyhow!("clickAt: failed to parse position JSON: {e}"))?;

            if parsed.get("error").is_some() {
                bail!("clickAt: element {eref} not found (run snapshot first)");
            }

            let cx = parsed.get("x").and_then(|v| v.as_f64())
                .ok_or_else(|| anyhow!("clickAt: missing x in bounding rect result"))?;
            let cy = parsed.get("y").and_then(|v| v.as_f64())
                .ok_or_else(|| anyhow!("clickAt: missing y in bounding rect result"))?;
            (cx, cy)
        } else if let (Some(x), Some(y)) = (explicit_x, explicit_y) {
            (x, y)
        } else {
            bail!("clickAt: `ref` or both `x` and `y` required");
        };

        // Dispatch three CDP mouse events with small delays to mimic a real click.
        self.cdp.send("Input.dispatchMouseEvent", json!({
            "type": "mouseMoved",
            "x": x,
            "y": y,
        })).await?;

        tokio::time::sleep(Duration::from_millis(50)).await;

        self.cdp.send("Input.dispatchMouseEvent", json!({
            "type": "mousePressed",
            "x": x,
            "y": y,
            "button": "left",
            "clickCount": 1,
        })).await?;

        tokio::time::sleep(Duration::from_millis(50)).await;

        self.cdp.send("Input.dispatchMouseEvent", json!({
            "type": "mouseReleased",
            "x": x,
            "y": y,
            "button": "left",
            "clickCount": 1,
        })).await?;

        Ok(json!({"action": "clickAt", "x": x, "y": y}))
    }

    async fn cmd_fill(&self, args: &Value) -> Result<Value> {
        let eref = args.get("ref").and_then(|v| v.as_str());
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("fill/type: `text` required"))?;

        if let Some(eref) = eref {
            // Fill a specific element with auto-wait.
            let escaped_text = escape_js_string(text);
            let js = format!(
                r#"(async function(){{
                    {FIND_REF_JS}
                    var el = findRef('{}');
                    if (!el) return 'NOT_FOUND';
                    {WAIT_ACTIONABLE_JS}
                    var status = await waitActionable(el, 5000);
                    if (status === 'TIMEOUT') return 'TIMEOUT';
                    el.focus();
                    el.value = '{}';
                    el.dispatchEvent(new Event('input', {{bubbles:true}}));
                    el.dispatchEvent(new Event('change', {{bubbles:true}}));
                    return 'OK';
                }})()"#,
                escape_js_string(eref),
                escaped_text,
                FIND_REF_JS = FIND_REF_JS,
                WAIT_ACTIONABLE_JS = WAIT_ACTIONABLE_JS,
            );

            let result = self.cdp.send("Runtime.evaluate", json!({
                "expression": js,
                "returnByValue": true,
                "awaitPromise": true,
            })).await?;
            let value = result.get("result").and_then(|r| r.get("value"))
                .and_then(|v| v.as_str()).unwrap_or("");

            match value {
                "NOT_FOUND" => bail!("fill: element {eref} not found (run snapshot first)"),
                "TIMEOUT" => bail!("fill: element {eref} not actionable within 5s"),
                _ => {}
            }

            Ok(json!({ "action": "fill", "ref": eref, "text": format!("Filled {eref} with text") }))
        } else {
            // No ref: type into the focused element via Input.dispatchKeyEvent.
            for ch in text.chars() {
                self.cdp
                    .send(
                        "Input.dispatchKeyEvent",
                        json!({
                            "type": "keyDown",
                            "text": ch.to_string(),
                        }),
                    )
                    .await?;
                self.cdp
                    .send(
                        "Input.dispatchKeyEvent",
                        json!({
                            "type": "keyUp",
                            "text": ch.to_string(),
                        }),
                    )
                    .await?;
            }
            Ok(json!({ "action": "type", "text": format!("Typed {} characters", text.len()) }))
        }
    }

    async fn cmd_select(&self, args: &Value) -> Result<Value> {
        let eref = args
            .get("ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("select: `ref` required"))?;
        let value = args
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("select: `value` required"))?;

        let js = format!(
            r#"(async function(){{
                {FIND_REF_JS}
                var el = findRef('{}');
                if (!el) return 'NOT_FOUND';
                {WAIT_ACTIONABLE_JS}
                var status = await waitActionable(el, 5000);
                if (status === 'TIMEOUT') return 'TIMEOUT';
                el.value = '{}';
                el.dispatchEvent(new Event('change', {{bubbles:true}}));
                return 'OK';
            }})()"#,
            escape_js_string(eref),
            escape_js_string(value),
            FIND_REF_JS = FIND_REF_JS,
            WAIT_ACTIONABLE_JS = WAIT_ACTIONABLE_JS,
        );

        let result = self.cdp.send("Runtime.evaluate", json!({
            "expression": js,
            "returnByValue": true,
            "awaitPromise": true,
        })).await?;
        let value_str = result.get("result").and_then(|r| r.get("value"))
            .and_then(|v| v.as_str()).unwrap_or("");

        match value_str {
            "NOT_FOUND" => bail!("select: element {eref} not found"),
            "TIMEOUT" => bail!("select: element {eref} not actionable within 5s"),
            _ => {}
        }

        Ok(json!({ "action": "select", "ref": eref, "text": format!("Selected {value} on {eref}") }))
    }

    async fn cmd_check(&self, args: &Value, check: bool) -> Result<Value> {
        let eref = args
            .get("ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("check/uncheck: `ref` required"))?;

        let desired = if check { "true" } else { "false" };
        let js = format!(
            r#"(async function(){{
                {FIND_REF_JS}
                var el = findRef('{}');
                if (!el) return 'NOT_FOUND';
                {WAIT_ACTIONABLE_JS}
                var status = await waitActionable(el, 5000);
                if (status === 'TIMEOUT') return 'TIMEOUT';
                if (el.checked !== {}) el.click();
                return 'OK';
            }})()"#,
            escape_js_string(eref),
            desired,
            FIND_REF_JS = FIND_REF_JS,
            WAIT_ACTIONABLE_JS = WAIT_ACTIONABLE_JS,
        );

        let result = self.cdp.send("Runtime.evaluate", json!({
            "expression": js,
            "returnByValue": true,
            "awaitPromise": true,
        })).await?;
        let value = result.get("result").and_then(|r| r.get("value"))
            .and_then(|v| v.as_str()).unwrap_or("");

        match value {
            "NOT_FOUND" => bail!("check: element {eref} not found"),
            "TIMEOUT" => bail!("check: element {eref} not actionable within 5s"),
            _ => {}
        }

        let verb = if check { "Checked" } else { "Unchecked" };
        Ok(json!({ "action": if check { "check" } else { "uncheck" }, "ref": eref, "text": format!("{verb} {eref}") }))
    }

    async fn cmd_scroll(&self, args: &Value) -> Result<Value> {
        let direction = args
            .get("direction")
            .and_then(|v| v.as_str())
            .unwrap_or("down");
        let amount = args
            .get("amount")
            .and_then(|v| v.as_i64())
            .unwrap_or(500);
        let selector = args.get("selector").and_then(|v| v.as_str());

        let (dx, dy) = match direction {
            "up" => (0, -amount),
            "down" => (0, amount),
            "left" => (-amount, 0),
            "right" => (amount, 0),
            _ => (0, amount),
        };

        let js = if let Some(sel) = selector {
            format!(
                r#"(function(){{var el=document.querySelector('{}');if(!el)return 'NOT_FOUND';el.scrollBy({dx},{dy});return 'OK';}})()"#,
                escape_js_string(sel)
            )
        } else {
            format!("window.scrollBy({dx}, {dy}); 'OK'")
        };

        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" {
            bail!("scroll: container `{}` not found", selector.unwrap_or(""));
        }

        Ok(json!({ "action": "scroll", "direction": direction, "amount": amount }))
    }

    /// Resolve a ref (@eN) or selector from args.
    fn get_ref_or_selector<'a>(&self, args: &'a Value) -> Option<&'a str> {
        args.get("ref").and_then(|v| v.as_str())
            .or_else(|| args.get("selector").and_then(|v| v.as_str()))
    }

    /// Build JS to find element by @ref or CSS selector.
    fn build_find_js(&self, ref_or_sel: &str) -> String {
        format!(
            r#"{FIND_REF_JS} var el=findRef('{}'); if(!el) el=document.querySelector('{}'); if(!el) return 'NOT_FOUND';"#,
            escape_js_string(ref_or_sel),
            escape_js_string(ref_or_sel),
            FIND_REF_JS = FIND_REF_JS,
        )
    }

    /// Get element center coordinates by ref/selector.
    async fn get_element_center(&self, ref_or_sel: &str) -> Result<(f64, f64)> {
        let find = self.build_find_js(ref_or_sel);
        let js = format!(
            r#"(function(){{{find} var r=el.getBoundingClientRect(); return JSON.stringify({{x:r.x+r.width/2,y:r.y+r.height/2}});}})() "#,
        );
        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" {
            bail!("element `{ref_or_sel}` not found (run snapshot first)");
        }
        let coords: Value = serde_json::from_str(&result)?;
        Ok((coords["x"].as_f64().unwrap_or(0.0), coords["y"].as_f64().unwrap_or(0.0)))
    }

    /// Hover over an element (triggers tooltips, dropdown menus).
    async fn cmd_hover(&self, args: &Value) -> Result<Value> {
        let sel = self.get_ref_or_selector(args).ok_or_else(|| anyhow!("hover: `ref` or `selector` required"))?;
        let (x, y) = self.get_element_center(sel).await?;
        self.cdp.send("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": x, "y": y})).await?;
        Ok(json!({"action": "hover", "ref": sel}))
    }

    /// Double-click an element.
    async fn cmd_dblclick(&self, args: &Value) -> Result<Value> {
        let sel = self.get_ref_or_selector(args).ok_or_else(|| anyhow!("dblclick: `ref` or `selector` required"))?;
        let (x, y) = self.get_element_center(sel).await?;
        self.cdp.send("Input.dispatchMouseEvent", json!({"type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 2})).await?;
        self.cdp.send("Input.dispatchMouseEvent", json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 2})).await?;
        Ok(json!({"action": "dblclick", "ref": sel}))
    }

    /// Drag from one element to another (for slider captchas, sorting, etc.).
    async fn cmd_drag(&self, args: &Value) -> Result<Value> {
        let from = args.get("from").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("drag: `from` ref required"))?;
        let to = args.get("to").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("drag: `to` ref required"))?;
        let (fx, fy) = self.get_element_center(from).await?;
        let (tx, ty) = self.get_element_center(to).await?;
        self.cdp.send("Input.dispatchMouseEvent", json!({"type": "mousePressed", "x": fx, "y": fy, "button": "left"})).await?;
        let steps = 10;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            self.cdp.send("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": fx + (tx-fx)*t, "y": fy + (ty-fy)*t})).await?;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        self.cdp.send("Input.dispatchMouseEvent", json!({"type": "mouseReleased", "x": tx, "y": ty, "button": "left"})).await?;
        Ok(json!({"action": "drag", "from": from, "to": to}))
    }

    /// Focus an element.
    async fn cmd_focus(&self, args: &Value) -> Result<Value> {
        let sel = self.get_ref_or_selector(args).ok_or_else(|| anyhow!("focus: `ref` or `selector` required"))?;
        let find = self.build_find_js(sel);
        let js = format!(r#"(function(){{{find} el.focus(); return 'OK';}})()"#);
        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" { bail!("focus: element `{sel}` not found"); }
        Ok(json!({"action": "focus", "ref": sel}))
    }

    /// Scroll an element into view.
    async fn cmd_scroll_into_view(&self, args: &Value) -> Result<Value> {
        let sel = self.get_ref_or_selector(args).ok_or_else(|| anyhow!("scrollintoview: `ref` or `selector` required"))?;
        let find = self.build_find_js(sel);
        let js = format!(r#"(function(){{{find} el.scrollIntoView({{behavior:'smooth',block:'center'}}); return 'OK';}})()"#);
        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" { bail!("scrollintoview: element `{sel}` not found"); }
        Ok(json!({"action": "scrollintoview", "ref": sel}))
    }

    /// Press a key down without releasing it.
    async fn cmd_keydown(&self, args: &Value) -> Result<Value> {
        let key = args.get("key").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("keydown: `key` required"))?;
        self.cdp.send("Input.dispatchKeyEvent", json!({
            "type": "keyDown",
            "key": key,
        })).await?;
        Ok(json!({"action": "keydown", "key": key}))
    }

    /// Release a previously pressed key.
    async fn cmd_keyup(&self, args: &Value) -> Result<Value> {
        let key = args.get("key").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("keyup: `key` required"))?;
        self.cdp.send("Input.dispatchKeyEvent", json!({
            "type": "keyUp",
            "key": key,
        })).await?;
        Ok(json!({"action": "keyup", "key": key}))
    }

    /// Raw mouse operation: move, click, down, or up at given coordinates.
    async fn cmd_mouse(&self, args: &Value) -> Result<Value> {
        let x = args.get("x").and_then(|v| v.as_f64())
            .ok_or_else(|| anyhow!("mouse: `x` required"))?;
        let y = args.get("y").and_then(|v| v.as_f64())
            .ok_or_else(|| anyhow!("mouse: `y` required"))?;
        let button = args.get("button").and_then(|v| v.as_str()).unwrap_or("left");
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("click");
        match action {
            "move" => {
                self.cdp.send("Input.dispatchMouseEvent", json!({
                    "type": "mouseMoved", "x": x, "y": y,
                })).await?;
            }
            "down" => {
                self.cdp.send("Input.dispatchMouseEvent", json!({
                    "type": "mousePressed", "x": x, "y": y, "button": button, "clickCount": 1,
                })).await?;
            }
            "up" => {
                self.cdp.send("Input.dispatchMouseEvent", json!({
                    "type": "mouseReleased", "x": x, "y": y, "button": button, "clickCount": 1,
                })).await?;
            }
            _ => {
                self.cdp.send("Input.dispatchMouseEvent", json!({
                    "type": "mousePressed", "x": x, "y": y, "button": button, "clickCount": 1,
                })).await?;
                self.cdp.send("Input.dispatchMouseEvent", json!({
                    "type": "mouseReleased", "x": x, "y": y, "button": button, "clickCount": 1,
                })).await?;
            }
        }
        Ok(json!({"action": "mouse", "x": x, "y": y, "button": button, "mouse_action": action}))
    }

    /// Read/write localStorage or sessionStorage.
    async fn cmd_storage(&self, args: &Value) -> Result<Value> {
        let op = args.get("value").and_then(|v| v.as_str()).unwrap_or("get");
        let storage_type = args.get("type").and_then(|v| v.as_str()).unwrap_or("local");
        let store = if storage_type == "session" { "sessionStorage" } else { "localStorage" };
        match op {
            "get" => {
                let key = args.get("key").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("storage get: `key` required"))?;
                let js = format!(r#"{}.getItem('{}')"#, store, escape_js_string(key));
                let val = self.eval_js(&js).await?;
                Ok(json!({"action": "storage", "op": "get", "key": key, "data": val}))
            }
            "set" => {
                let key = args.get("key").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("storage set: `key` required"))?;
                let data = args.get("data").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("storage set: `data` required"))?;
                let js = format!(r#"{}.setItem('{}', '{}')"#, store, escape_js_string(key), escape_js_string(data));
                self.eval_js(&js).await?;
                Ok(json!({"action": "storage", "op": "set", "key": key}))
            }
            "clear" => {
                let js = format!("{}.clear()", store);
                self.eval_js(&js).await?;
                Ok(json!({"action": "storage", "op": "clear", "type": storage_type}))
            }
            other => Err(anyhow!("storage: unsupported op `{other}`, use get/set/clear")),
        }
    }

    /// Wait for a download to complete.
    async fn cmd_download_wait(&self, args: &Value) -> Result<Value> {
        let timeout_secs = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
        self.cdp.send("Page.setDownloadBehavior", json!({
            "behavior": "allow",
            "downloadPath": "/tmp/rsclaw-downloads",
        })).await?;
        tokio::time::sleep(Duration::from_secs(timeout_secs)).await;
        Ok(json!({"action": "download_wait", "timeout": timeout_secs, "status": "completed"}))
    }

    /// Query element state: visible, hidden, checked, enabled, disabled.
    async fn cmd_is(&self, args: &Value) -> Result<Value> {
        let sel = self.get_ref_or_selector(args).ok_or_else(|| anyhow!("is: `ref` or `selector` required"))?;
        let check = args.get("check").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("is: `check` required (visible/hidden/checked/enabled/disabled)"))?;
        let find = self.build_find_js(sel);
        let js_check = match check {
            "visible" => "var r=el.getBoundingClientRect(); return String(r.width>0 && r.height>0 && getComputedStyle(el).visibility!=='hidden');",
            "hidden" => "var r=el.getBoundingClientRect(); return String(r.width===0 || r.height===0 || getComputedStyle(el).visibility==='hidden' || getComputedStyle(el).display==='none');",
            "checked" => "return String(!!el.checked);",
            "enabled" => "return String(!el.disabled);",
            "disabled" => "return String(!!el.disabled);",
            other => bail!("is: unsupported check `{other}`"),
        };
        let js = format!(r#"(function(){{{find} {js_check}}})()"#);
        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" { bail!("is: element `{sel}` not found"); }
        let value = result == "true";
        Ok(json!({"action": "is", "ref": sel, "check": check, "result": value}))
    }

    /// Get an element attribute value (text, value, href, src, class, or any attribute).
    async fn cmd_get(&self, args: &Value) -> Result<Value> {
        let sel = self.get_ref_or_selector(args).ok_or_else(|| anyhow!("get: `ref` or `selector` required"))?;
        let attr = args.get("attr").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("get: `attr` required (text/value/href/src/class/...)"))?;
        let find = self.build_find_js(sel);
        let js_attr = match attr {
            "text" => "return el.textContent || '';".to_string(),
            "value" => "return el.value || '';".to_string(),
            _ => format!("return el.getAttribute('{}') || '';", escape_js_string(attr)),
        };
        let js = format!(r#"(function(){{{find} {js_attr}}})()"#);
        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" { bail!("get: element `{sel}` not found"); }
        Ok(json!({"action": "get", "ref": sel, "attr": attr, "value": result}))
    }

    async fn cmd_screenshot(&mut self, args: &Value) -> Result<Value> {
        let format = args.get("format").and_then(|v| v.as_str()).unwrap_or("png");
        let quality = args.get("quality").and_then(|v| v.as_i64());
        let full_page = args.get("full_page").and_then(|v| v.as_bool()).unwrap_or(false);
        let annotate = args.get("annotate").and_then(|v| v.as_bool()).unwrap_or(false);

        let mut params = json!({ "format": format });
        if let Some(q) = quality {
            params["quality"] = json!(q);
        }
        if full_page {
            let dims = self.eval_js(
                "JSON.stringify({w: document.documentElement.scrollWidth, h: document.documentElement.scrollHeight})"
            ).await?;
            if let Ok(d) = serde_json::from_str::<Value>(&dims) {
                let w = d["w"].as_f64().unwrap_or(1280.0);
                let h = d["h"].as_f64().unwrap_or(720.0);
                params["clip"] = json!({ "x": 0, "y": 0, "width": w, "height": h, "scale": 1 });
                params["captureBeyondViewport"] = json!(true);
            }
        }

        let mime = if format == "jpeg" { "image/jpeg" } else { "image/png" };

        // Annotated screenshot: overlay numbered labels on interactive elements
        if annotate {
            self.cmd_snapshot(&json!({})).await?;

            let annotate_js = r#"(function(){
                var refs=document.querySelectorAll('[data-ref]');var labels=[];
                refs.forEach(function(el){var ref=el.getAttribute('data-ref');
                var num=ref.replace('@e','');var rect=el.getBoundingClientRect();
                var label=document.createElement('div');label.className='__rsclaw_annotation';
                label.textContent=num;label.style.cssText='position:fixed;z-index:999999;background:red;color:white;font-size:11px;font-weight:bold;padding:1px 4px;border-radius:8px;pointer-events:none;left:'+(rect.left-4)+'px;top:'+(rect.top-4)+'px;';
                document.body.appendChild(label);
                labels.push({num:parseInt(num),ref:ref,tag:el.tagName.toLowerCase(),text:(el.innerText||el.value||el.alt||'').substring(0,50)});});
                return JSON.stringify(labels);
            })()"#;

            let legend_raw = self.eval_js(annotate_js).await?;
            let result = self.cdp.send("Page.captureScreenshot", params).await?;
            let data = result.get("data").and_then(|v| v.as_str()).unwrap_or("");

            // Remove annotations
            let _ = self.eval_js("document.querySelectorAll('.__rsclaw_annotation').forEach(e=>e.remove())").await;

            let legend: Value = serde_json::from_str(&legend_raw).unwrap_or(json!([]));
            return Ok(json!({
                "action": "screenshot",
                "image": format!("data:{mime};base64,{data}"),
                "legend": legend,
            }));
        }

        let result = self.cdp.send("Page.captureScreenshot", params).await?;
        let data = result.get("data").and_then(|v| v.as_str()).unwrap_or("");

        Ok(json!({
            "action": "screenshot",
            "image": format!("data:{mime};base64,{data}")
        }))
    }

    async fn cmd_pdf(&self) -> Result<Value> {
        let result = self.cdp.send("Page.printToPDF", json!({})).await?;

        let data = result
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        Ok(json!({
            "action": "pdf",
            "data": format!("data:application/pdf;base64,{data}")
        }))
    }

    async fn cmd_back(&self) -> Result<Value> {
        self.eval_js("history.back()").await?;
        time::sleep(Duration::from_millis(500)).await;
        Ok(json!({ "action": "back", "text": "Navigated back" }))
    }

    async fn cmd_forward(&self) -> Result<Value> {
        self.eval_js("history.forward()").await?;
        time::sleep(Duration::from_millis(500)).await;
        Ok(json!({ "action": "forward", "text": "Navigated forward" }))
    }

    async fn cmd_reload(&self) -> Result<Value> {
        self.cdp.send("Page.reload", json!({})).await?;
        // Wait for load event.
        let _ = self.cdp.wait_event("Page.loadEventFired", 15).await;
        Ok(json!({ "action": "reload", "text": "Page reloaded" }))
    }

    async fn cmd_get_text(&self) -> Result<Value> {
        let text = self.eval_js("document.body.innerText").await?;
        let truncated = if text.len() > 50_000 {
            text[..50_000].to_owned()
        } else {
            text
        };
        Ok(json!({ "action": "get_text", "text": truncated }))
    }

    async fn cmd_get_url(&self) -> Result<Value> {
        let url = self.eval_js("location.href").await?;
        Ok(json!({ "action": "get_url", "url": url }))
    }

    async fn cmd_get_title(&self) -> Result<Value> {
        let title = self.eval_js("document.title").await?;
        Ok(json!({ "action": "get_title", "title": title }))
    }

    async fn cmd_wait(&self, args: &Value) -> Result<Value> {
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("element");
        let value = args
            .get("value")
            .or_else(|| args.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let timeout_secs = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(15);

        let js = match target {
            "url" => format!(r#"location.href.includes('{}')"#, escape_js_string(value)),
            "text" => format!(
                r#"document.body.innerText.includes('{}')"#,
                escape_js_string(value)
            ),
            "networkidle" => {
                r#"(function(){var entries=performance.getEntriesByType('resource');if(entries.length===0)return true;var last=entries[entries.length-1];return(performance.now()-last.responseEnd)>500;})()"#.to_string()
            }
            "fn" | "js" | "function" => value.to_string(),
            // Default: wait for an element matching a CSS selector.
            _ => format!(
                r#"!!document.querySelector('{}')"#,
                escape_js_string(value)
            ),
        };

        let deadline = time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let result = self.eval_js(&js).await?;
            if result == "true" {
                return Ok(
                    json!({ "action": "wait", "target": target, "text": format!("Wait condition met: {target}={value}") }),
                );
            }
            if time::Instant::now() >= deadline {
                bail!("wait: timed out after {timeout_secs}s waiting for {target}={value}");
            }
            time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn cmd_evaluate(&self, args: &Value) -> Result<Value> {
        let js = args
            .get("js")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("evaluate: `js` required"))?;

        let result = self
            .cdp
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": js,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;

        let value = result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null);

        Ok(json!({ "action": "evaluate", "result": value }))
    }

    async fn cmd_cookies(&self, args: &Value) -> Result<Value> {
        let sub_action = args
            .get("value")
            .or_else(|| args.get("cookies_action"))
            .and_then(|v| v.as_str())
            .unwrap_or("get");

        match sub_action {
            "get" => {
                let result = self.cdp.send("Network.getCookies", json!({})).await?;
                Ok(json!({ "action": "cookies", "cookies": result.get("cookies") }))
            }
            "set" => {
                let cookie = args.get("cookie").cloned().unwrap_or(json!({}));
                self.cdp.send("Network.setCookie", cookie).await?;
                Ok(json!({ "action": "cookies", "text": "Cookie set" }))
            }
            "clear" => {
                self.cdp
                    .send("Network.clearBrowserCookies", json!({}))
                    .await?;
                Ok(json!({ "action": "cookies", "text": "Cookies cleared" }))
            }
            other => Err(anyhow!(
                "cookies: unknown sub-action `{other}` (use get/set/clear)"
            )),
        }
    }

    // -- New actions (Phase 1-3) -----------------------------------------------

    async fn cmd_press(&self, args: &Value) -> Result<Value> {
        let key = args.get("key").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("press: `key` required"))?;

        let lower = key.to_lowercase();
        let (key_code, code, text): (i32, String, String) = match lower.as_str() {
            "enter" | "return" => (13, "Enter".into(), "\r".into()),
            "tab" => (9, "Tab".into(), "\t".into()),
            "escape" | "esc" => (27, "Escape".into(), String::new()),
            "backspace" => (8, "Backspace".into(), String::new()),
            "delete" => (46, "Delete".into(), String::new()),
            "arrowup" | "up" => (38, "ArrowUp".into(), String::new()),
            "arrowdown" | "down" => (40, "ArrowDown".into(), String::new()),
            "arrowleft" | "left" => (37, "ArrowLeft".into(), String::new()),
            "arrowright" | "right" => (39, "ArrowRight".into(), String::new()),
            "space" => (32, "Space".into(), " ".into()),
            "home" => (36, "Home".into(), String::new()),
            "end" => (35, "End".into(), String::new()),
            "pageup" => (33, "PageUp".into(), String::new()),
            "pagedown" => (34, "PageDown".into(), String::new()),
            "f1" => (112, "F1".into(), String::new()),
            "f2" => (113, "F2".into(), String::new()),
            "f3" => (114, "F3".into(), String::new()),
            "f4" => (115, "F4".into(), String::new()),
            "f5" => (116, "F5".into(), String::new()),
            "f6" => (117, "F6".into(), String::new()),
            "f7" => (118, "F7".into(), String::new()),
            "f8" => (119, "F8".into(), String::new()),
            "f9" => (120, "F9".into(), String::new()),
            "f10" => (121, "F10".into(), String::new()),
            "f11" => (122, "F11".into(), String::new()),
            "f12" => (123, "F12".into(), String::new()),
            other => {
                // Single printable character
                let ch = other.chars().next().unwrap_or('\0');
                let vk = if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_uppercase() as i32
                } else {
                    0
                };
                (vk, key.to_string(), if ch.is_ascii() && !ch.is_ascii_control() { ch.to_string() } else { String::new() })
            }
        };

        self.cdp.send("Input.dispatchKeyEvent", json!({
            "type": "keyDown",
            "key": code,
            "windowsVirtualKeyCode": key_code,
            "nativeVirtualKeyCode": key_code,
            "text": text,
        })).await?;
        self.cdp.send("Input.dispatchKeyEvent", json!({
            "type": "keyUp",
            "key": code,
            "windowsVirtualKeyCode": key_code,
            "nativeVirtualKeyCode": key_code,
        })).await?;

        Ok(json!({ "action": "press", "key": key }))
    }

    async fn cmd_set_viewport(&self, args: &Value) -> Result<Value> {
        let width = args.get("width").and_then(|v| v.as_u64()).unwrap_or(1280) as u32;
        let height = args.get("height").and_then(|v| v.as_u64()).unwrap_or(720) as u32;
        let scale = args.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0);
        let mobile = args.get("mobile").and_then(|v| v.as_bool()).unwrap_or(false);

        self.cdp.send("Emulation.setDeviceMetricsOverride", json!({
            "width": width,
            "height": height,
            "deviceScaleFactor": scale,
            "mobile": mobile,
        })).await?;

        Ok(json!({ "action": "set_viewport", "width": width, "height": height, "scale": scale }))
    }

    async fn cmd_dialog(&mut self, args: &Value) -> Result<Value> {
        let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("accept");
        match sub {
            "accept" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let mut params = json!({"accept": true});
                if !text.is_empty() {
                    params["promptText"] = json!(text);
                }
                match self.cdp.send("Page.handleJavaScriptDialog", params).await {
                    Ok(_) => {
                        self.pending_dialog = None;
                        Ok(json!({"action": "dialog", "text": "Dialog accepted"}))
                    }
                    Err(_) => Ok(json!({"action": "dialog", "text": "No dialog open to accept"})),
                }
            }
            "dismiss" => {
                match self.cdp.send("Page.handleJavaScriptDialog", json!({"accept": false})).await {
                    Ok(_) => {
                        self.pending_dialog = None;
                        Ok(json!({"action": "dialog", "text": "Dialog dismissed"}))
                    }
                    Err(_) => Ok(json!({"action": "dialog", "text": "No dialog open to dismiss"})),
                }
            }
            "status" => {
                Ok(json!({"action": "dialog", "pending": self.pending_dialog.is_some(),
                           "message": self.pending_dialog.as_deref().unwrap_or("")}))
            }
            _ => Err(anyhow!("dialog: unknown sub-action (use accept/dismiss/status)"))
        }
    }

    async fn cmd_new_tab(&self, args: &Value) -> Result<Value> {
        let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("about:blank");
        let result = self.cdp.send("Target.createTarget", json!({"url": url})).await?;
        let target_id = result.get("targetId").and_then(|v| v.as_str()).unwrap_or("");
        Ok(json!({"action": "new_tab", "targetId": target_id}))
    }

    async fn cmd_list_tabs(&self) -> Result<Value> {
        let port = self.debug_port;
        let url = format!("http://127.0.0.1:{port}/json");
        let targets: Vec<Value> = reqwest::get(&url).await?.json().await?;
        let tabs: Vec<Value> = targets.iter()
            .filter(|t| t["type"].as_str() == Some("page"))
            .map(|t| json!({
                "id": t["id"],
                "title": t["title"],
                "url": t["url"],
            }))
            .collect();
        Ok(json!({"action": "list_tabs", "tabs": tabs}))
    }

    async fn cmd_switch_tab(&mut self, args: &Value) -> Result<Value> {
        let target_id = args.get("target_id").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("switch_tab: `target_id` required"))?;

        self.cdp.send("Target.activateTarget", json!({"targetId": target_id})).await?;

        let port = self.debug_port;
        let url = format!("http://127.0.0.1:{port}/json");
        let targets: Vec<Value> = reqwest::get(&url).await?.json().await?;
        let target = targets.iter()
            .find(|t| t["id"].as_str() == Some(target_id))
            .ok_or_else(|| anyhow!("switch_tab: target not found"))?;
        let ws_url = target["webSocketDebuggerUrl"].as_str()
            .ok_or_else(|| anyhow!("switch_tab: no WebSocket URL"))?;

        let new_cdp = CdpClient::connect(ws_url).await?;
        new_cdp.send("Page.enable", json!({})).await?;
        new_cdp.send("DOM.enable", json!({})).await?;
        new_cdp.send("Runtime.enable", json!({})).await?;
        new_cdp.send("Network.enable", json!({})).await?;

        self.cdp = new_cdp;
        self.refs.clear();
        self.ref_counter = 0;

        Ok(json!({"action": "switch_tab", "target_id": target_id}))
    }

    async fn cmd_close_tab(&mut self, args: &Value) -> Result<Value> {
        let target_id = args.get("target_id").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("close_tab: `target_id` required"))?;

        // Detect if this is the active tab by checking the current CDP ws_url.
        // If closing the active tab, switch to another tab first.
        let port = self.debug_port;
        let url = format!("http://127.0.0.1:{port}/json");
        let targets: Vec<Value> = reqwest::get(&url).await?.json().await?;
        let is_active = targets.iter().any(|t| {
            t["id"].as_str() == Some(target_id) &&
            t["webSocketDebuggerUrl"].as_str().map(|ws| self.cdp.ws_url() == ws).unwrap_or(false)
        });

        if is_active {
            // Find another tab to switch to
            let other = targets.iter()
                .find(|t| t["type"].as_str() == Some("page") && t["id"].as_str() != Some(target_id));
            if let Some(other_target) = other {
                let other_id = other_target["id"].as_str().unwrap_or("");
                self.cmd_switch_tab(&json!({"target_id": other_id})).await?;
            } else {
                bail!("close_tab: cannot close the only remaining tab");
            }
        }

        self.cdp.send("Target.closeTarget", json!({"targetId": target_id})).await?;
        Ok(json!({"action": "close_tab", "target_id": target_id}))
    }

    async fn cmd_state(&mut self, args: &Value) -> Result<Value> {
        let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("save");
        match sub {
            "save" => {
                let cookies_result = self.cdp.send("Network.getCookies", json!({})).await?;
                let cookies = cookies_result.get("cookies").cloned().unwrap_or(json!([]));
                let local_storage = self.eval_js("JSON.stringify(Object.assign({}, localStorage))").await?;
                let session_storage = self.eval_js("JSON.stringify(Object.assign({}, sessionStorage))").await?;
                let url = self.eval_js("location.href").await?;

                let state = json!({
                    "url": url,
                    "cookies": cookies,
                    "localStorage": serde_json::from_str::<Value>(&local_storage).unwrap_or(json!({})),
                    "sessionStorage": serde_json::from_str::<Value>(&session_storage).unwrap_or(json!({})),
                });

                Ok(json!({"action": "state", "sub": "save", "state": state}))
            }
            "load" => {
                let state = args.get("state")
                    .ok_or_else(|| anyhow!("state load: `state` object required"))?;

                if let Some(cookies) = state.get("cookies").and_then(|v| v.as_array()) {
                    for cookie in cookies {
                        let _ = self.cdp.send("Network.setCookie", cookie.clone()).await;
                    }
                }

                if let Some(url) = state.get("url").and_then(|v| v.as_str()) {
                    self.cdp.send("Page.navigate", json!({"url": url})).await?;
                    let _ = self.cdp.wait_event("Page.loadEventFired", 15).await;
                }

                if let Some(ls) = state.get("localStorage").and_then(|v| v.as_object()) {
                    for (k, v) in ls {
                        let val = v.as_str().unwrap_or("");
                        let _ = self.eval_js(&format!(
                            "localStorage.setItem('{}', '{}')",
                            escape_js_string(k), escape_js_string(val)
                        )).await;
                    }
                }

                if let Some(ss) = state.get("sessionStorage").and_then(|v| v.as_object()) {
                    for (k, v) in ss {
                        let val = v.as_str().unwrap_or("");
                        let _ = self.eval_js(&format!(
                            "sessionStorage.setItem('{}', '{}')",
                            escape_js_string(k), escape_js_string(val)
                        )).await;
                    }
                }

                self.cdp.send("Page.reload", json!({})).await?;
                let _ = self.cdp.wait_event("Page.loadEventFired", 15).await;

                Ok(json!({"action": "state", "sub": "load", "text": "State restored"}))
            }
            _ => Err(anyhow!("state: unknown sub-action (use save/load)"))
        }
    }

    async fn cmd_network(&mut self, args: &Value) -> Result<Value> {
        let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("requests");
        match sub {
            "requests" => {
                let js = r#"JSON.stringify(
                    performance.getEntriesByType('resource').slice(-50).map(e=>({
                        name:e.name,type:e.initiatorType,
                        duration:Math.round(e.duration),
                        size:e.transferSize||0
                    }))
                )"#;
                let result = self.eval_js(js).await?;
                let entries: Value = serde_json::from_str(&result).unwrap_or(json!([]));
                Ok(json!({"action": "network", "requests": entries}))
            }
            "block" => {
                let pattern = args.get("pattern").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("network block: `pattern` required"))?;
                if !self.blocked_urls.contains(&pattern.to_string()) {
                    self.blocked_urls.push(pattern.to_string());
                }
                self.cdp.send("Network.setBlockedURLs", json!({"urls": self.blocked_urls})).await?;
                Ok(json!({"action": "network", "text": format!("Blocking {} pattern(s)", self.blocked_urls.len())}))
            }
            "unblock" => {
                self.blocked_urls.clear();
                self.cdp.send("Network.setBlockedURLs", json!({"urls": []})).await?;
                Ok(json!({"action": "network", "text": "All URL blocks removed"}))
            }
            "intercept" => {
                let pattern = args.get("pattern").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("network intercept: `pattern` required"))?;
                let action = args.get("action_type").and_then(|v| v.as_str()).unwrap_or("block");
                let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");

                if self.intercept_rules.is_empty() {
                    self.cdp.send("Fetch.enable", json!({"patterns": [{"urlPattern": "*"}]})).await?;
                }

                let rule_action = if action == "mock" {
                    format!("mock:{body}")
                } else {
                    action.to_string()
                };
                self.intercept_rules.push((pattern.to_string(), rule_action));

                Ok(json!({"action": "network", "text": format!("Intercept rule added: {pattern} -> {action}")}))
            }
            "clear_intercepts" => {
                self.intercept_rules.clear();
                let _ = self.cdp.send("Fetch.disable", json!({})).await;
                Ok(json!({"action": "network", "text": "All intercept rules cleared"}))
            }
            "sniff" | "resources" => {
                // Discover all media resources on the page: images, videos, audio,
                // fonts, and XHR/fetch URLs — combines DOM scanning with performance API.
                let filter = args.get("text").and_then(|v| v.as_str()).unwrap_or("all");
                let js = format!(r#"(function() {{
  var r = {{}};
  // DOM: images
  document.querySelectorAll('img,picture source').forEach(function(el){{
    var s = el.src || el.dataset.src || el.currentSrc || el.getAttribute('srcset') || '';
    if(s && s.startsWith('http')) {{ r[s] = r[s] || {{type:'image',tag:el.tagName}}; }}
  }});
  // DOM: videos
  document.querySelectorAll('video,video source').forEach(function(el){{
    var s = el.src || el.dataset.src || el.getAttribute('src') || '';
    if(s && s.startsWith('http')) {{ r[s] = r[s] || {{type:'video',tag:el.tagName}}; }}
  }});
  // DOM: audio
  document.querySelectorAll('audio,audio source').forEach(function(el){{
    var s = el.src || el.dataset.src || '';
    if(s && s.startsWith('http')) {{ r[s] = r[s] || {{type:'audio',tag:el.tagName}}; }}
  }});
  // DOM: stylesheets and scripts
  document.querySelectorAll('link[rel=stylesheet]').forEach(function(el){{
    if(el.href) {{ r[el.href] = r[el.href] || {{type:'css'}}; }}
  }});
  // Performance API: XHR/fetch/other resources
  performance.getEntriesByType('resource').forEach(function(e){{
    if(!r[e.name]) {{
      var t = e.initiatorType || 'other';
      if(t==='xmlhttprequest'||t==='fetch') t='xhr';
      r[e.name] = {{type:t,size:e.transferSize||0}};
    }}
  }});
  // Filter
  var filter = '{filter}';
  var entries = Object.entries(r);
  if(filter !== 'all') {{
    entries = entries.filter(function(kv){{ return kv[1].type === filter; }});
  }}
  return JSON.stringify(entries.map(function(kv){{
    return {{url:kv[0],type:kv[1].type,tag:kv[1].tag||'',size:kv[1].size||0}};
  }}));
}})()"#);
                let result = self.eval_js(&js).await?;
                let resources: Value = serde_json::from_str(&result).unwrap_or(json!([]));
                let count = resources.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(json!({"action":"network","sub":"sniff","filter":filter,"count":count,"resources":resources}))
            }
            _ => Err(anyhow!("network: unknown sub-action (use requests/sniff/block/unblock/intercept/clear_intercepts)"))
        }
    }

    async fn cmd_highlight(&self, args: &Value) -> Result<Value> {
        let eref = args.get("ref").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("highlight: `ref` required"))?;
        let js = format!(
            r#"(function(){{var el=document.querySelector('[data-ref="{}"]');if(!el)return 'NOT_FOUND';el.style.outline='3px solid red';el.style.outlineOffset='2px';el.scrollIntoView({{block:'center'}});return 'OK';}})()"#,
            escape_js_string(eref)
        );
        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" { bail!("highlight: {eref} not found"); }
        Ok(json!({"action": "highlight", "ref": eref}))
    }

    async fn cmd_clipboard(&self, args: &Value) -> Result<Value> {
        // Grant clipboard permissions to avoid rejection in headless mode.
        let _ = self.cdp.send("Browser.grantPermissions", json!({
            "permissions": ["clipboardReadWrite", "clipboardSanitizedWrite"]
        })).await;

        let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("read");
        match sub {
            "read" => {
                let result = self.cdp.send("Runtime.evaluate", json!({
                    "expression": "navigator.clipboard.readText()",
                    "awaitPromise": true,
                    "returnByValue": true,
                    "userGesture": true,
                })).await?;
                let text = result.get("result")
                    .and_then(|r| r.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Ok(json!({"action": "clipboard", "text": text}))
            }
            "write" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let result = self.cdp.send("Runtime.evaluate", json!({
                    "expression": format!("navigator.clipboard.writeText('{}')", escape_js_string(text)),
                    "awaitPromise": true,
                    "returnByValue": true,
                    "userGesture": true,
                })).await?;
                // Check for exception
                if result.get("exceptionDetails").is_some() {
                    bail!("clipboard write failed (may not be supported in headless mode)");
                }
                Ok(json!({"action": "clipboard", "text": "Written to clipboard"}))
            }
            _ => Err(anyhow!("clipboard: use read/write"))
        }
    }

    async fn cmd_find(&self, args: &Value) -> Result<Value> {
        let by = args.get("by").and_then(|v| v.as_str()).unwrap_or("text");
        let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
        let then = args.get("then").and_then(|v| v.as_str());

        let js = match by {
            "text" => format!(
                r#"(function(){{var t='{}';var all=document.querySelectorAll('a,button,[role=button],[role=link]');for(var i=0;i<all.length;i++){{if(all[i].innerText&&all[i].innerText.trim().includes(t)){{all[i].scrollIntoView({{block:'center'}});return JSON.stringify({{found:true,tag:all[i].tagName,text:all[i].innerText.substring(0,100)}});}}}};var everything=document.querySelectorAll('*');for(var i=0;i<everything.length;i++){{var s=window.getComputedStyle(everything[i]);if(s.cursor==='pointer'&&everything[i].innerText&&everything[i].innerText.trim().includes(t)){{everything[i].scrollIntoView({{block:'center'}});return JSON.stringify({{found:true,tag:everything[i].tagName,text:everything[i].innerText.substring(0,100)}});}}}};return JSON.stringify({{found:false}});}})()"#,
                escape_js_string(value)
            ),
            "label" => format!(
                r#"(function(){{var labels=document.querySelectorAll('label');for(var i=0;i<labels.length;i++){{if(labels[i].textContent.includes('{}')){{var input=labels[i].querySelector('input,select,textarea')||document.getElementById(labels[i].getAttribute('for'));if(input){{input.scrollIntoView({{block:'center'}});return JSON.stringify({{found:true,tag:input.tagName}});}}}}}};return JSON.stringify({{found:false}});}})()"#,
                escape_js_string(value)
            ),
            _ => return Err(anyhow!("find: `by` must be text or label")),
        };

        let result_str = self.eval_js(&js).await?;
        let result: Value = serde_json::from_str(&result_str).unwrap_or(json!({"found": false}));

        if result["found"].as_bool() == Some(true) {
            if let Some("click") = then {
                if by == "text" {
                    let click_js = format!(
                        r#"(function(){{var t='{}';var all=document.querySelectorAll('a,button,[role=button],[role=link]');for(var i=0;i<all.length;i++){{if(all[i].innerText&&all[i].innerText.trim().includes(t)){{all[i].click();return 'OK';}}}};var everything=document.querySelectorAll('*');for(var i=0;i<everything.length;i++){{var s=window.getComputedStyle(everything[i]);if(s.cursor==='pointer'&&everything[i].innerText&&everything[i].innerText.trim().includes(t)){{everything[i].click();return 'OK';}}}};return 'NOT_FOUND';}})()"#,
                        escape_js_string(value)
                    );
                    self.eval_js(&click_js).await?;
                }
            }
        }

        Ok(json!({"action": "find", "by": by, "value": value, "result": result}))
    }

    async fn cmd_upload(&self, args: &Value) -> Result<Value> {
        let eref = args.get("ref").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("upload: `ref` required (file input element)"))?;
        let files: Vec<String> = args.get("files")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if files.is_empty() {
            bail!("upload: `files` array required");
        }

        // Get the backend node ID via DOM.querySelector
        let doc_root = self.cdp.send("DOM.getDocument", json!({})).await?;
        let root_id = doc_root["root"]["nodeId"].as_i64().unwrap_or(0);
        let node_result = self.cdp.send("DOM.querySelector", json!({
            "nodeId": root_id,
            "selector": format!("[data-ref=\"{}\"]", escape_js_string(eref)),
        })).await?;
        let node_id = node_result["nodeId"].as_i64().unwrap_or(0);
        if node_id == 0 {
            bail!("upload: could not resolve DOM node for {eref}");
        }

        self.cdp.send("DOM.setFileInputFiles", json!({
            "files": files,
            "nodeId": node_id,
        })).await?;

        Ok(json!({"action": "upload", "ref": eref, "files": files.len()}))
    }

    async fn cmd_get_article(&self) -> Result<Value> {
        let js = r#"(function(){
            var doc=document.cloneNode(true);var body=doc.querySelector('body');
            if(!body)return JSON.stringify({title:'',content:'',text:''});
            var noise='nav,header,footer,aside,.sidebar,.nav,.menu,.breadcrumb,.pagination,'+
                '.cookie-banner,.modal,.popup,.ad,.ads,.advertisement,[role=navigation],'+
                '[role=banner],[role=contentinfo],script,style,noscript,svg,iframe,'+
                '.social-share,.share-buttons,.related-posts,.comments,#comments,'+
                '.newsletter,.subscribe,.signup-form';
            doc.querySelectorAll(noise).forEach(function(el){el.remove();});
            var selectors=['article','[role=main]','main','.post-content','.article-content',
                '.entry-content','.content','#content','.post','#main'];
            var article=null;
            for(var i=0;i<selectors.length;i++){
                var el=doc.querySelector(selectors[i]);
                if(el&&el.innerText.trim().length>200){article=el;break;}
            }
            if(!article){
                var best=null,bestLen=0;
                doc.querySelectorAll('div,section').forEach(function(el){
                    var len=el.innerText.trim().length;
                    var ratio=len/(el.innerHTML.length||1);
                    var score=len*ratio;
                    if(score>bestLen){bestLen=score;best=el;}
                });
                article=best||body;
            }
            var title=(doc.querySelector('h1')||doc.querySelector('title')||{}).innerText||document.title||'';
            var text=article.innerText.trim();
            var links=[];
            article.querySelectorAll('a[href]').forEach(function(a){
                var t=a.innerText.trim();
                if(t&&t.length>2)links.push({text:t.substring(0,100),href:a.href});
            });
            var images=[];
            article.querySelectorAll('img[src]').forEach(function(img){
                images.push({src:img.src,alt:img.alt||''});
            });
            if(text.length>50000)text=text.substring(0,50000)+'...(truncated)';
            return JSON.stringify({
                title:title.substring(0,200),text:text,
                links:links.slice(0,50),images:images.slice(0,20),
                length:text.length
            });
        })()"#;

        let result_str = self.eval_js(js).await?;
        let result: Value = serde_json::from_str(&result_str)
            .unwrap_or_else(|_| json!({"title":"","text":"","links":[],"images":[]}));

        Ok(json!({
            "action": "get_article",
            "title": result["title"],
            "text": result["text"],
            "links": result["links"],
            "images": result["images"],
            "length": result["length"],
        }))
    }

    // -- Helpers ----------------------------------------------------------------

    /// Evaluate a JS expression and return its string value.
    async fn eval_js(&self, expression: &str) -> Result<String> {
        let result = self
            .cdp
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                }),
            )
            .await?;

        let value = result
            .get("result")
            .and_then(|r| r.get("value"));

        match value {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(v) => Ok(v.to_string()),
            None => Ok(String::new()),
        }
    }

    // -- Task 4: Cookie Isolation -----------------------------------------------

    async fn cmd_context(&mut self, args: &Value) -> Result<Value> {
        let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("new");
        match sub {
            "new" => {
                let result = self.cdp.send("Target.createBrowserContext", json!({})).await?;
                let ctx_id = result["browserContextId"].as_str().unwrap_or("");
                Ok(json!({"action": "context", "sub": "new", "browserContextId": ctx_id}))
            }
            "dispose" => {
                let ctx_id = args.get("context_id").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("context dispose: `context_id` required"))?;
                self.cdp.send("Target.disposeBrowserContext", json!({"browserContextId": ctx_id})).await?;
                Ok(json!({"action": "context", "sub": "dispose"}))
            }
            "new_tab" => {
                let ctx_id = args.get("context_id").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("context new_tab: `context_id` required"))?;
                let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("about:blank");
                let result = self.cdp.send("Target.createTarget", json!({"url": url, "browserContextId": ctx_id})).await?;
                let target_id = result["targetId"].as_str().unwrap_or("");
                Ok(json!({"action": "context", "sub": "new_tab", "targetId": target_id}))
            }
            _ => Err(anyhow!("context: use new/dispose/new_tab"))
        }
    }

    // -- Task 5: Geolocation & Emulation ----------------------------------------

    async fn cmd_emulate(&self, args: &Value) -> Result<Value> {
        let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("geo");
        match sub {
            "geo" => {
                let lat = args.get("latitude").and_then(|v| v.as_f64())
                    .ok_or_else(|| anyhow!("emulate geo: `latitude` required"))?;
                let lon = args.get("longitude").and_then(|v| v.as_f64())
                    .ok_or_else(|| anyhow!("emulate geo: `longitude` required"))?;
                let accuracy = args.get("accuracy").and_then(|v| v.as_f64()).unwrap_or(1.0);
                self.cdp.send("Emulation.setGeolocationOverride", json!({
                    "latitude": lat, "longitude": lon, "accuracy": accuracy
                })).await?;
                let _ = self.cdp.send("Browser.grantPermissions", json!({"permissions": ["geolocation"]})).await;
                Ok(json!({"action": "emulate", "sub": "geo", "latitude": lat, "longitude": lon}))
            }
            "locale" => {
                let locale = args.get("locale").and_then(|v| v.as_str()).unwrap_or("en-US");
                self.cdp.send("Emulation.setLocaleOverride", json!({"locale": locale})).await?;
                Ok(json!({"action": "emulate", "sub": "locale", "locale": locale}))
            }
            "timezone" => {
                let tz = args.get("timezone_id").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("emulate timezone: `timezone_id` required"))?;
                self.cdp.send("Emulation.setTimezoneOverride", json!({"timezoneId": tz})).await?;
                Ok(json!({"action": "emulate", "sub": "timezone", "timezone_id": tz}))
            }
            "permission" => {
                let perms: Vec<String> = args.get("permissions")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                self.cdp.send("Browser.grantPermissions", json!({"permissions": perms})).await?;
                Ok(json!({"action": "emulate", "sub": "permission", "granted": perms}))
            }
            _ => Err(anyhow!("emulate: use geo/locale/timezone/permission"))
        }
    }

    // -- Task 6: Screenshot Diff ------------------------------------------------

    async fn cmd_diff(&mut self, args: &Value) -> Result<Value> {
        let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("mark");
        match sub {
            "mark" => {
                let result = self.cdp.send("Page.captureScreenshot", json!({"format": "png"})).await?;
                let data = result.get("data").and_then(|v| v.as_str()).unwrap_or("");
                self.before_screenshot = Some(data.to_owned());
                Ok(json!({"action": "diff", "sub": "mark", "text": "Baseline screenshot captured"}))
            }
            "compare" => {
                let before = self.before_screenshot.as_deref()
                    .ok_or_else(|| anyhow!("diff compare: call diff mark first"))?;
                let after_result = self.cdp.send("Page.captureScreenshot", json!({"format": "png"})).await?;
                let after = after_result.get("data").and_then(|v| v.as_str()).unwrap_or("");
                let changed = before != after;
                let before_bytes = before.len();
                let after_bytes = after.len();
                let diff_ratio = if before_bytes == 0 { 1.0 } else {
                    let common = before.bytes().zip(after.bytes()).filter(|(a, b)| a == b).count();
                    1.0 - (common as f64 / before_bytes.max(after_bytes) as f64)
                };
                Ok(json!({
                    "action": "diff", "sub": "compare", "changed": changed,
                    "diff_ratio": format!("{:.1}%", diff_ratio * 100.0),
                    "before_image": format!("data:image/png;base64,{before}"),
                    "after_image": format!("data:image/png;base64,{after}"),
                }))
            }
            _ => Err(anyhow!("diff: use mark/compare"))
        }
    }

    // -- Task 7: Operation Recording --------------------------------------------

    async fn cmd_record(&mut self, args: &Value) -> Result<Value> {
        let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("start");
        match sub {
            "start" => {
                self.recording = Some(Vec::new());
                Ok(json!({"action": "record", "sub": "start", "text": "Recording started"}))
            }
            "stop" => {
                let entries = self.recording.take().unwrap_or_default();
                let count = entries.len();
                Ok(json!({"action": "record", "sub": "stop", "operations": count, "trace": entries}))
            }
            "status" => {
                let active = self.recording.is_some();
                let count = self.recording.as_ref().map(|e| e.len()).unwrap_or(0);
                Ok(json!({"action": "record", "sub": "status", "active": active, "operations": count}))
            }
            _ => Err(anyhow!("record: use start/stop/status"))
        }
    }

    /// Universal site search: navigate to a URL, auto-detect search input,
    /// fill query text, submit, and return the page text.
    ///
    /// Works on any site (Douyin, Taobao, JD, Xiaohongshu, Baidu, Google, etc.)
    /// by probing common search input patterns.
    async fn cmd_search(&mut self, args: &Value) -> Result<Value> {
        let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let text = args.get("text").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("search: `text` required"))?;
        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(15);

        // Navigate to the target site if URL provided.
        if !url.is_empty() {
            self.cmd_open(&json!({"url": url})).await?;
            // Wait for page to be interactive.
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }

        let escaped_text = escape_js_string(text);

        // Auto-detect search input, fill, and submit.
        let search_js = format!(r#"(function() {{
            // Priority-ordered selectors for search inputs.
            var selectors = [
                'input[type="search"]',
                'input[name="q"]',
                'input[name="query"]',
                'input[name="keyword"]',
                'input[name="wd"]',
                'input[name="search"]',
                'input[name="kw"]',
                'input[name="key"]',
                'input[name="text"]',
                'input[aria-label*="search" i]',
                'input[aria-label*="搜索" i]',
                'input[placeholder*="search" i]',
                'input[placeholder*="搜索" i]',
                'input[placeholder*="查找" i]',
                'input[placeholder*="输入" i]',
                'textarea[name="q"]',
                'textarea[name="query"]',
                'input[type="text"][class*="search" i]',
                'input[type="text"][id*="search" i]',
                'input[type="text"][class*="query" i]',
                'input[type="text"][id*="query" i]',
                'input[type="text"][id*="kw"]',
                'input[type="text"]'
            ];

            var input = null;
            for (var i = 0; i < selectors.length; i++) {{
                var el = document.querySelector(selectors[i]);
                if (el && el.offsetParent !== null) {{
                    input = el;
                    break;
                }}
            }}

            if (!input) {{
                return JSON.stringify({{ok: false, error: 'no search input found'}});
            }}

            // Focus and fill.
            input.focus();
            input.value = '';
            var nativeInputValueSetter = Object.getOwnPropertyDescriptor(
                window.HTMLInputElement.prototype, 'value'
            )?.set || Object.getOwnPropertyDescriptor(
                window.HTMLTextAreaElement.prototype, 'value'
            )?.set;
            if (nativeInputValueSetter) {{
                nativeInputValueSetter.call(input, '{escaped_text}');
            }} else {{
                input.value = '{escaped_text}';
            }}
            input.dispatchEvent(new Event('input', {{bubbles: true}}));
            input.dispatchEvent(new Event('change', {{bubbles: true}}));

            // Try to find and click submit button.
            var submitted = false;
            var btnSelectors = [
                'button[type="submit"]',
                'input[type="submit"]',
                'button[class*="search" i]',
                'button[class*="submit" i]',
                'button[aria-label*="search" i]',
                'button[aria-label*="搜索" i]',
                'a[class*="search" i][href*="search"]',
                '.search-btn',
                '.btn-search',
                '#search-btn',
                '#su'
            ];

            // Also check buttons near the input.
            var form = input.closest('form');
            if (form) {{
                var formBtn = form.querySelector('button, input[type="submit"]');
                if (formBtn) {{
                    formBtn.click();
                    submitted = true;
                }}
            }}

            if (!submitted) {{
                for (var j = 0; j < btnSelectors.length; j++) {{
                    var btn = document.querySelector(btnSelectors[j]);
                    if (btn && btn.offsetParent !== null) {{
                        btn.click();
                        submitted = true;
                        break;
                    }}
                }}
            }}

            // Fallback: press Enter on the input.
            if (!submitted) {{
                input.dispatchEvent(new KeyboardEvent('keydown', {{key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true}}));
                input.dispatchEvent(new KeyboardEvent('keyup', {{key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true}}));
                // Also try form submit.
                if (form) {{
                    try {{ form.submit(); }} catch(e) {{}}
                }}
            }}

            return JSON.stringify({{ok: true, submitted: submitted, selector: input.tagName + (input.name ? '[name=' + input.name + ']' : '')}});
        }})()"#);

        let result = self.cdp.send("Runtime.evaluate", json!({
            "expression": search_js,
            "returnByValue": true,
        })).await?;

        let result_str = result["result"]["value"].as_str().unwrap_or("{}");
        let parsed: Value = serde_json::from_str(result_str).unwrap_or_default();

        if parsed["ok"].as_bool() != Some(true) {
            let err = parsed["error"].as_str().unwrap_or("unknown error");
            return Ok(json!({"action": "search", "ok": false, "error": err}));
        }

        // Wait for results page to load.
        let _ = tokio::time::timeout(
            Duration::from_secs(timeout),
            self.cdp.wait_event("Page.loadEventFired", timeout),
        ).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Return page text content.
        let page_text = self.cmd_get_text().await?;
        let page_url = self.cmd_get_url().await?;
        let page_title = self.cmd_get_title().await?;

        Ok(json!({
            "action": "search",
            "ok": true,
            "url": page_url["url"],
            "title": page_title["title"],
            "text": page_text["text"],
            "input": parsed["selector"],
        }))
    }

    // -----------------------------------------------------------------------
    // Console — get browser console messages
    // -----------------------------------------------------------------------

    /// Get recent console messages (log, warn, error, info).
    async fn cmd_console(&self, args: &Value) -> Result<Value> {
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let level = args.get("level").and_then(|v| v.as_str()).unwrap_or("all");

        let js = r#"(function(){
            if (!window.__rsclaw_console) return JSON.stringify([]);
            return JSON.stringify(window.__rsclaw_console.slice(-500));
        })()"#;

        // Inject console interceptor if not already done.
        let inject = r#"(function(){
            if (window.__rsclaw_console) return;
            window.__rsclaw_console = [];
            var orig = {log: console.log, warn: console.warn, error: console.error, info: console.info};
            ['log','warn','error','info'].forEach(function(level) {
                console[level] = function() {
                    var args = Array.from(arguments).map(function(a) {
                        try { return typeof a === 'object' ? JSON.stringify(a) : String(a); }
                        catch(e) { return String(a); }
                    });
                    window.__rsclaw_console.push({level: level, text: args.join(' '), ts: Date.now()});
                    if (window.__rsclaw_console.length > 500) window.__rsclaw_console.shift();
                    orig[level].apply(console, arguments);
                };
            });
        })()"#;

        let _ = self.cdp.send("Runtime.evaluate", json!({"expression": inject})).await;
        let result = self.cdp.send("Runtime.evaluate", json!({"expression": js})).await?;
        let raw = result["result"]["value"].as_str().unwrap_or("[]");
        let entries: Vec<Value> = serde_json::from_str(raw).unwrap_or_default();

        let filtered: Vec<&Value> = entries.iter()
            .filter(|e| level == "all" || e["level"].as_str() == Some(level))
            .rev()
            .take(limit)
            .collect();

        Ok(json!({"action": "console", "entries": filtered, "count": filtered.len()}))
    }

    // -----------------------------------------------------------------------
    // Content — get full page HTML
    // -----------------------------------------------------------------------

    /// Get the full HTML content of the current page.
    async fn cmd_content(&self) -> Result<Value> {
        let result = self.cdp.send("Runtime.evaluate", json!({
            "expression": "document.documentElement.outerHTML"
        })).await?;
        let html = result["result"]["value"].as_str().unwrap_or("");
        Ok(json!({"action": "content", "html": html, "length": html.len()}))
    }

    // -----------------------------------------------------------------------
    // Frame — switch to iframe / mainframe
    // -----------------------------------------------------------------------

    /// Switch execution context to an iframe by selector or @ref.
    async fn cmd_frame(&mut self, args: &Value) -> Result<Value> {
        let selector = args.get("selector").or(args.get("ref"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("frame: `selector` or `ref` required"))?;

        // Resolve @ref to CSS selector if needed.
        let css = if selector.starts_with('@') {
            let idx: usize = selector.trim_start_matches("@e").parse()
                .map_err(|_| anyhow!("frame: invalid ref `{selector}`"))?;
            format!("[data-rsclaw-ref='e{idx}']")
        } else {
            selector.to_owned()
        };

        // Get the iframe's frame ID via Page.getFrameTree.
        let _tree = self.cdp.send("Page.getFrameTree", json!({})).await?;

        // Find iframe src from DOM to match with frame tree.
        let js = format!(
            r#"(function(){{
                var el = document.querySelector('{css}');
                if (!el) return JSON.stringify({{"error": "element not found"}});
                if (el.tagName !== 'IFRAME') return JSON.stringify({{"error": "not an iframe"}});
                return JSON.stringify({{"src": el.src || '', "name": el.name || ''}});
            }})()"#,
            css = css.replace('\'', "\\'")
        );
        let result = self.cdp.send("Runtime.evaluate", json!({"expression": js})).await?;
        let raw = result["result"]["value"].as_str().unwrap_or("{}");
        let parsed: Value = serde_json::from_str(raw).unwrap_or_default();

        if parsed.get("error").is_some() {
            return Ok(parsed);
        }

        Ok(json!({
            "action": "frame",
            "selector": selector,
            "iframe": parsed,
            "hint": "Use evaluate with the iframe's document context for interactions inside the frame"
        }))
    }

    /// Switch back to the main frame.
    async fn cmd_mainframe(&self) -> Result<Value> {
        // Mainframe is the default execution context — no special action needed
        // as CDP Runtime.evaluate runs in the main context by default.
        Ok(json!({"action": "mainframe", "status": "switched to main frame"}))
    }

    // -----------------------------------------------------------------------
    // WaitForUrl — wait for URL to change
    // -----------------------------------------------------------------------

    /// Wait for the page URL to match a pattern (useful after login/redirect).
    async fn cmd_wait_for_url(&self, args: &Value) -> Result<Value> {
        let pattern = args.get("url").or(args.get("pattern"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("waitforurl: `url` pattern required"))?;
        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);

        let start = std::time::Instant::now();
        let deadline = std::time::Duration::from_secs(timeout);

        loop {
            let result = self.cdp.send("Runtime.evaluate", json!({
                "expression": "window.location.href"
            })).await?;
            let current_url = result["result"]["value"].as_str().unwrap_or("");

            if current_url.contains(pattern) {
                return Ok(json!({
                    "action": "waitforurl",
                    "matched": true,
                    "url": current_url,
                    "pattern": pattern
                }));
            }

            if start.elapsed() > deadline {
                return Ok(json!({
                    "action": "waitforurl",
                    "matched": false,
                    "url": current_url,
                    "pattern": pattern,
                    "error": "timeout"
                }));
            }

            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    // -----------------------------------------------------------------------
    // Semantic locators — getbytext, getbyrole, getbylabel
    // -----------------------------------------------------------------------

    /// Find elements by semantic locators (text, role, label).
    async fn cmd_getby(&self, args: &Value, by: &str) -> Result<Value> {
        let value = args.get("value").or(args.get("text")).or(args.get("role")).or(args.get("label"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("getby{by}: `value` required"))?;
        let exact = args.get("exact").and_then(|v| v.as_bool()).unwrap_or(false);

        let escaped = escape_js_string(value);

        let js = match by {
            "text" => format!(
                r#"(function(){{
                    var exact = {exact};
                    var query = '{escaped}';
                    var walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
                    var results = [];
                    while (walker.nextNode()) {{
                        var text = walker.currentNode.textContent.trim();
                        var match = exact ? text === query : text.toLowerCase().includes(query.toLowerCase());
                        if (match && walker.currentNode.parentElement) {{
                            var el = walker.currentNode.parentElement;
                            var tag = el.tagName.toLowerCase();
                            var r = el.getBoundingClientRect();
                            results.push({{tag: tag, text: text.substring(0, 100), x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height)}});
                            if (results.length >= 10) break;
                        }}
                    }}
                    return JSON.stringify(results);
                }})()"#
            ),
            "role" => format!(
                r#"(function(){{
                    var els = document.querySelectorAll('[role="{escaped}"]');
                    var results = [];
                    els.forEach(function(el) {{
                        var r = el.getBoundingClientRect();
                        var text = (el.textContent || '').trim().substring(0, 100);
                        results.push({{tag: el.tagName.toLowerCase(), role: '{escaped}', text: text, x: Math.round(r.x), y: Math.round(r.y)}});
                        if (results.length >= 10) return;
                    }});
                    return JSON.stringify(results);
                }})()"#
            ),
            "label" => format!(
                r#"(function(){{
                    var labels = document.querySelectorAll('label');
                    var results = [];
                    labels.forEach(function(label) {{
                        var text = (label.textContent || '').trim();
                        if (text.toLowerCase().includes('{escaped}'.toLowerCase())) {{
                            var forId = label.getAttribute('for');
                            var input = forId ? document.getElementById(forId) : label.querySelector('input,select,textarea');
                            if (input) {{
                                var r = input.getBoundingClientRect();
                                results.push({{label: text.substring(0, 100), tag: input.tagName.toLowerCase(), type: input.type || '', x: Math.round(r.x), y: Math.round(r.y)}});
                            }}
                        }}
                    }});
                    return JSON.stringify(results);
                }})()"#
            ),
            _ => return Err(anyhow!("getby: unknown locator type `{by}`")),
        };

        let result = self.cdp.send("Runtime.evaluate", json!({"expression": js})).await?;
        let raw = result["result"]["value"].as_str().unwrap_or("[]");
        let elements: Vec<Value> = serde_json::from_str(raw).unwrap_or_default();

        Ok(json!({
            "action": format!("getby{by}"),
            "value": value,
            "elements": elements,
            "count": elements.len()
        }))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Escape a string for embedding in a JS string literal (single-quoted).
fn escape_js_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\0', "\\0")
        .replace('\'', "\\'")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace(']', "\\]")
}

// ---------------------------------------------------------------------------
// Snapshot JS -- injected into the page to build an accessibility-like tree
// ---------------------------------------------------------------------------

/// JS helper to find an element by data-ref, including inside same-origin iframes.
const FIND_REF_JS: &str = r#"function findRef(ref){var el=document.querySelector('[data-ref="'+ref+'"]');if(el)return el;var iframes=document.querySelectorAll('iframe');for(var i=0;i<iframes.length;i++){try{var doc=iframes[i].contentDocument;if(doc){el=doc.querySelector('[data-ref="'+ref+'"]');if(el)return el;}}catch(e){}}return null;}"#;

/// JS helper: wait for element to be visible, enabled, and position-stable.
const WAIT_ACTIONABLE_JS: &str = r#"function waitActionable(el,ms){return new Promise(function(resolve){var deadline=Date.now()+(ms||5000);var lastRect=null;function check(){if(Date.now()>deadline){resolve('TIMEOUT');return;}var rect=el.getBoundingClientRect();var style=getComputedStyle(el);var visible=rect.width>0&&rect.height>0&&style.visibility!=='hidden'&&style.display!=='none';var enabled=!el.disabled;var stable=lastRect&&Math.abs(rect.top-lastRect.top)<2&&Math.abs(rect.left-lastRect.left)<2;lastRect=rect;if(visible&&enabled&&stable){resolve('OK');}else{setTimeout(check,50);}}check();});}"#;

const SNAPSHOT_JS: &str = r#"(function(){
  var lines = [];
  var counter = 0;
  var INTERACTIVE_ROLES = ['button','link','textbox','checkbox','radio','tab',
    'menuitem','menuitemcheckbox','menuitemradio','switch','slider','combobox',
    'searchbox','spinbutton','option','treeitem'];
  function walk(node, depth) {
    if (node.nodeType === 3) {
      var text = node.textContent.trim();
      if (text) {
        var t = text.length > 200 ? text.substring(0, 200) + '...' : text;
        lines.push('  '.repeat(depth) + t);
      }
      return;
    }
    if (node.nodeType !== 1) return;
    var el = node;
    var tag = el.tagName.toLowerCase();
    if (tag === 'script' || tag === 'style' || tag === 'noscript') return;
    // Skip hidden elements to reduce noise.
    var style = getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden') return;
    if (el.offsetWidth === 0 && el.offsetHeight === 0 && tag !== 'input') return;
    var role = el.getAttribute('role') || '';
    var ariaLabel = el.getAttribute('aria-label') || '';
    var isEditable = el.isContentEditable && !el.parentElement.isContentEditable;
    var hasCursorPointer = style.cursor === 'pointer';
    // Detect upload/dropzone areas.
    var cls = (el.className || '').toString().toLowerCase();
    var isUploadZone = (tag === 'input' && el.type === 'file')
      || role === 'dropzone'
      || cls.indexOf('upload') >= 0 || cls.indexOf('dropzone') >= 0 || cls.indexOf('drop-area') >= 0
      || (tag === 'input' && el.getAttribute('accept'));
    // Detect rich text editors (Draft.js, Quill, Slate, ProseMirror, TinyMCE, Tiptap).
    var isRichEditor = isEditable && (
      cls.indexOf('ql-editor') >= 0 || cls.indexOf('DraftEditor') >= 0
      || cls.indexOf('slate-') >= 0 || cls.indexOf('ProseMirror') >= 0
      || cls.indexOf('tiptap') >= 0 || cls.indexOf('mce-content') >= 0
      || cls.indexOf('editor') >= 0 || cls.indexOf('rich-text') >= 0
      || el.getAttribute('data-slate-editor') || el.getAttribute('data-contents')
    );
    // Detect chat input (textarea or editable near a send button).
    var isChatInput = (tag === 'textarea' || isEditable) && (
      cls.indexOf('chat') >= 0 || cls.indexOf('message') >= 0 || cls.indexOf('prompt') >= 0
      || (el.getAttribute('placeholder') || '').match(/[\u8f93\u5165\u53d1\u9001]|send|type|message|ask|chat/i)
    );
    var isInteractive = ['a','button','input','select','textarea','details','summary'].indexOf(tag) >= 0
      || INTERACTIVE_ROLES.indexOf(role) >= 0
      || isEditable || isUploadZone
      || el.getAttribute('onclick') || el.getAttribute('tabindex')
      || (hasCursorPointer && (el.innerText||'').trim().length > 0);
    var isDisabled = el.disabled || el.getAttribute('aria-disabled') === 'true';
    var ref = '';
    if (isInteractive && !isDisabled) {
      counter++;
      ref = '@e' + counter;
      el.setAttribute('data-ref', ref);
    }
    var label = '';
    if (isUploadZone && tag === 'input') label = 'upload[file]';
    else if (isUploadZone) label = 'upload-zone';
    else if (tag === 'a') label = 'link';
    else if (tag === 'button' || role === 'button') label = 'button';
    else if (tag === 'input') label = 'input[' + (el.type||'text') + ']';
    else if (tag === 'select') label = 'select';
    else if (isChatInput && tag === 'textarea') label = 'chat-input';
    else if (tag === 'textarea') label = 'textarea';
    else if (isRichEditor) label = 'rich-editor';
    else if (isChatInput && isEditable) label = 'chat-input';
    else if (isEditable) label = 'editable';
    else if (tag === 'img') label = 'img';
    else if (tag === 'video') label = 'video';
    else if (tag === 'h1'||tag === 'h2'||tag === 'h3'||tag === 'h4'||tag === 'h5'||tag === 'h6') label = tag;
    else if (['nav','main','header','footer','aside','section','article','form'].indexOf(tag) >= 0) label = tag;
    else if (hasCursorPointer && isInteractive) label = 'clickable';
    else label = '';

    var text = ariaLabel || el.getAttribute('alt') || el.getAttribute('placeholder') || el.getAttribute('title') || '';
    if (!text && isInteractive) {
      var inner = el.innerText;
      if (inner) text = inner.split('\n')[0].substring(0, 100);
    }
    // Find associated label for form inputs.
    if (!text && el.id && (tag === 'input' || tag === 'select' || tag === 'textarea')) {
      var lbl = document.querySelector('label[for="' + el.id + '"]');
      if (lbl) text = lbl.innerText.substring(0, 80);
    }

    if (label || ref) {
      var prefix = '  '.repeat(depth);
      var refStr = ref ? ' ' + ref : '';
      var disStr = isDisabled ? ' [disabled]' : '';
      var textStr = text ? ' "' + text.substring(0, 100) + '"' : '';
      var extraStr = '';
      if ((tag === 'input' || tag === 'textarea' || isEditable) && (el.value || el.innerText)) {
        var val = el.value || el.innerText;
        extraStr = ' value="' + val.substring(0, 50) + '"';
      }
      if (tag === 'select' && el.selectedOptions && el.selectedOptions.length > 0) {
        extraStr = ' selected="' + el.selectedOptions[0].text.substring(0, 50) + '"';
      }
      if (tag === 'a' && el.href) {
        var href = el.href.length > 80 ? el.href.substring(0, 80) + '...' : el.href;
        extraStr += ' href="' + href + '"';
      }
      if (isUploadZone) {
        var accept = el.getAttribute('accept') || '';
        if (accept) extraStr += ' accept="' + accept + '"';
        var multiple = el.hasAttribute('multiple');
        if (multiple) extraStr += ' [multiple]';
      }
      if (tag === 'img') {
        extraStr = ' ' + (el.naturalWidth||el.width||0) + 'x' + (el.naturalHeight||el.height||0);
        if (el.src) {
          var src = el.src.length > 60 ? el.src.substring(0, 60) + '...' : el.src;
          extraStr += ' src="' + src + '"';
        }
      }
      lines.push(prefix + '[' + label + ']' + refStr + disStr + textStr + extraStr);
    }
    if (tag === 'iframe') {
      try {
        var iframeDoc = el.contentDocument || el.contentWindow.document;
        if (iframeDoc && iframeDoc.body) {
          lines.push('  '.repeat(depth + 1) + '[iframe-content]');
          walk(iframeDoc.body, depth + 2);
        }
      } catch(e) {
        lines.push('  '.repeat(depth + 1) + '[iframe: cross-origin]');
      }
    }
    for (var child = node.firstChild; child; child = child.nextSibling) {
      walk(child, label ? depth + 1 : depth);
    }
  }
  if (document.body) walk(document.body, 0);
  return JSON.stringify({lines: lines, refCount: counter});
})()"#;

/// Interactive-only snapshot: only outputs elements that have @ref (interactive).
/// Saves ~80% tokens compared to full snapshot.
const SNAPSHOT_INTERACTIVE_JS: &str = r#"(function(){
  var lines = [];
  var counter = 0;
  var INTERACTIVE_ROLES = ['button','link','textbox','checkbox','radio','tab',
    'menuitem','menuitemcheckbox','menuitemradio','switch','slider','combobox',
    'searchbox','spinbutton','option','treeitem'];
  function walk(node, depth) {
    if (node.nodeType !== 1) return;
    var el = node;
    var tag = el.tagName.toLowerCase();
    if (tag === 'script' || tag === 'style' || tag === 'noscript') return;
    var style = getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden') return;
    if (el.offsetWidth === 0 && el.offsetHeight === 0 && tag !== 'input') return;
    var role = el.getAttribute('role') || '';
    var ariaLabel = el.getAttribute('aria-label') || '';
    var isEditable = el.isContentEditable && !el.parentElement.isContentEditable;
    var hasCursorPointer = style.cursor === 'pointer';
    var cls = (el.className || '').toString().toLowerCase();
    var isUploadZone = (tag === 'input' && el.type === 'file')
      || role === 'dropzone' || cls.indexOf('upload') >= 0;
    var isInteractive = ['a','button','input','select','textarea','details','summary'].indexOf(tag) >= 0
      || INTERACTIVE_ROLES.indexOf(role) >= 0
      || isEditable || isUploadZone
      || el.getAttribute('onclick') || el.getAttribute('tabindex')
      || (hasCursorPointer && (el.innerText||'').trim().length > 0);
    var isDisabled = el.disabled || el.getAttribute('aria-disabled') === 'true';
    if (isInteractive && !isDisabled) {
      counter++;
      var ref = '@e' + counter;
      el.setAttribute('data-ref', ref);
      var label = '';
      if (isUploadZone && tag === 'input') label = 'upload[file]';
      else if (tag === 'a') label = 'link';
      else if (tag === 'button' || role === 'button') label = 'button';
      else if (tag === 'input') label = 'input[' + (el.type||'text') + ']';
      else if (tag === 'select') label = 'select';
      else if (tag === 'textarea') label = 'textarea';
      else if (isEditable) label = 'editable';
      else label = 'clickable';
      var text = ariaLabel || el.getAttribute('alt') || el.getAttribute('placeholder') || el.getAttribute('title') || '';
      if (!text) {
        var inner = el.innerText;
        if (inner) text = inner.split('\n')[0].substring(0, 100);
      }
      var extraStr = '';
      if ((tag === 'input' || tag === 'textarea' || isEditable) && (el.value || el.innerText)) {
        extraStr = ' value="' + (el.value || el.innerText).substring(0, 50) + '"';
      }
      if (tag === 'a' && el.href) {
        var href = el.href.length > 80 ? el.href.substring(0, 80) + '...' : el.href;
        extraStr += ' href="' + href + '"';
      }
      var textStr = text ? ' "' + text.substring(0, 100) + '"' : '';
      lines.push('[' + label + '] ' + ref + textStr + extraStr);
    }
    for (var child = node.firstChild; child; child = child.nextSibling) {
      walk(child, depth + 1);
    }
  }
  if (document.body) walk(document.body, 0);
  return JSON.stringify({lines: lines, refCount: counter});
})()"#;
