//! CDP (Chrome DevTools Protocol) browser automation.
//!
//! Provides `BrowserSession` -- a high-level API that launches a headless
//! Chrome process, connects via WebSocket, and exposes actions such as
//! navigate, snapshot (accessibility-like tree), click, fill, screenshot, etc.

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

/// Compute max Chrome instances based on total system memory.
/// Rule: 1 instance per 2 GB, minimum 1.
fn max_instances() -> u32 {
    let total = total_system_memory_bytes();
    ((total / (2 * 1024 * 1024 * 1024)) as u32).max(1)
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

struct ChromeProcess {
    child: tokio::process::Child,
    ws_url: String,
    _tmp_dir: tempfile::TempDir,
}

impl ChromeProcess {
    async fn launch(chrome_path: &str, headed: bool) -> Result<Self> {
        can_launch_chrome()?;

        let tmp_dir = tempfile::tempdir()
            .map_err(|e| anyhow!("failed to create temp dir for Chrome profile: {e}"))?;

        let mut args = vec![
            "--disable-gpu",
            "--no-sandbox",
            "--disable-extensions",
            "--remote-debugging-port=0",
            "--window-size=1280,720",
        ];
        if !headed {
            args.push("--headless=new");
        }
        // Headed mode needs an initial URL to ensure a page target is created.
        args.push("about:blank");

        let mut child = tokio::process::Command::new(chrome_path)
            .args(&args)
            .arg(format!("--user-data-dir={}", tmp_dir.path().display()))
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

        let ws_url = time::timeout(Duration::from_secs(10), async {
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
    fn port(&self) -> Result<u16> {
        // ws://127.0.0.1:PORT/devtools/browser/...
        let url = &self.ws_url;
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

// ---------------------------------------------------------------------------
// CdpClient -- WebSocket CDP transport
// ---------------------------------------------------------------------------

struct CdpClient {
    ws_url: String,
    ws_tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Value>>>>,
    events_rx: Mutex<mpsc::UnboundedReceiver<Value>>,
    next_id: AtomicU32,
}

impl CdpClient {
    async fn connect(ws_url: &str) -> Result<Self> {
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
    async fn send(&self, method: &str, params: Value) -> Result<Value> {
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
    async fn wait_event(&self, event_method: &str, timeout_secs: u64) -> Result<Value> {
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
    /// Chrome process handle (killed on drop).
    chrome: ChromeProcess,
    cdp: CdpClient,
    /// @eN -> data-ref string mapping (kept in sync with snapshot).
    refs: HashMap<String, String>,
    /// Counter for next ref ID.
    ref_counter: u32,
    /// Chrome binary path (for restart).
    chrome_path: String,
    /// Run with visible window.
    pub headed: bool,
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
    /// Launch Chrome, discover the default page target, and connect CDP.
    pub async fn start(chrome_path: &str, headed: bool) -> Result<Self> {
        let chrome = ChromeProcess::launch(chrome_path, headed).await?;
        let cdp = Self::connect_cdp(&chrome).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Ok(Self {
            chrome,
            cdp,
            refs: HashMap::new(),
            ref_counter: 0,
            chrome_path: chrome_path.to_owned(),
            headed,
            pending_dialog: None,
            blocked_urls: Vec::new(),
            intercept_rules: Vec::new(),
            last_activity: Arc::new(AtomicU64::new(now)),
            before_screenshot: None,
            recording: None,
        })
    }

    /// Connect CDP to a Chrome process's page target.
    /// Retries discovery up to 10 times (headed mode can be slow to initialize).
    async fn connect_cdp(chrome: &ChromeProcess) -> Result<CdpClient> {
        let port = chrome.port()?;
        let discovery_url = format!("http://127.0.0.1:{port}/json");

        let mut page_target: Option<Value> = None;
        for attempt in 0..10 {
            if let Ok(resp) = reqwest::get(&discovery_url).await {
                if let Ok(targets) = resp.json::<Vec<Value>>().await {
                    if let Some(target) = targets.into_iter().find(|t| t["type"].as_str() == Some("page")) {
                        page_target = Some(target);
                        break;
                    }
                }
            }
            if attempt < 9 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        let page_target = page_target
            .ok_or_else(|| anyhow!("no page target found in CDP target list after 10 attempts"))?;

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
    pub fn is_alive(&mut self) -> bool {
        matches!(self.chrome.child.try_wait(), Ok(None))
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
    async fn restart(&mut self) -> Result<()> {
        warn!("restarting Chrome browser session");
        // Drop old chrome (kills process via Drop)
        let new_chrome = ChromeProcess::launch(&self.chrome_path, self.headed).await?;
        let new_cdp = Self::connect_cdp(&new_chrome).await?;
        self.chrome = new_chrome;
        self.cdp = new_cdp;
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
            "snapshot" => self.cmd_snapshot().await,
            "click" => self.cmd_click(args).await,
            "fill" | "type" => self.cmd_fill(args).await,
            "select" => self.cmd_select(args).await,
            "check" => self.cmd_check(args, true).await,
            "uncheck" => self.cmd_check(args, false).await,
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

    async fn cmd_snapshot(&mut self) -> Result<Value> {
        // Clear old refs.
        self.refs.clear();
        self.ref_counter = 0;

        let js = SNAPSHOT_JS;
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

        let lines = parsed
            .get("lines")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

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
            "text": lines,
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
            self.cmd_snapshot().await?;

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
        let port = self.chrome.port()?;
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

        let port = self.chrome.port()?;
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
        let port = self.chrome.port()?;
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
            _ => Err(anyhow!("network: unknown sub-action (use requests/block/unblock/intercept/clear_intercepts)"))
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
    var role = el.getAttribute('role') || '';
    var ariaLabel = el.getAttribute('aria-label') || '';
    var isInteractive = ['a','button','input','select','textarea','details','summary'].indexOf(tag) >= 0
      || role === 'button' || role === 'link' || role === 'textbox' || role === 'checkbox'
      || el.getAttribute('onclick') || el.getAttribute('tabindex');
    var ref = '';
    if (isInteractive) {
      counter++;
      ref = '@e' + counter;
      el.setAttribute('data-ref', ref);
    }
    var label = '';
    if (tag === 'a') label = 'link';
    else if (tag === 'button' || role === 'button') label = 'button';
    else if (tag === 'input') label = 'input[' + (el.type||'text') + ']';
    else if (tag === 'select') label = 'select';
    else if (tag === 'textarea') label = 'textarea';
    else if (tag === 'img') label = 'img';
    else if (tag === 'h1'||tag === 'h2'||tag === 'h3'||tag === 'h4'||tag === 'h5'||tag === 'h6') label = tag;
    else if (['nav','main','header','footer','aside','section','article','form'].indexOf(tag) >= 0) label = tag;
    else label = '';

    var text = ariaLabel || el.getAttribute('alt') || el.getAttribute('placeholder') || el.getAttribute('title') || '';
    if (!text && isInteractive) {
      var inner = el.innerText;
      if (inner) text = inner.split('\n')[0].substring(0, 100);
    }

    if (label || ref) {
      var prefix = '  '.repeat(depth);
      var refStr = ref ? ' ' + ref : '';
      var textStr = text ? ' "' + text.substring(0, 100) + '"' : '';
      var valueStr = '';
      if ((tag === 'input' || tag === 'textarea') && el.value) {
        valueStr = ' value="' + el.value.substring(0, 50) + '"';
      }
      lines.push(prefix + '[' + label + ']' + refStr + textStr + valueStr);
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
