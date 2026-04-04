//! CDP (Chrome DevTools Protocol) browser automation.
//!
//! Provides `BrowserSession` -- a high-level API that launches a headless
//! Chrome process, connects via WebSocket, and exposes actions such as
//! navigate, snapshot (accessibility-like tree), click, fill, screenshot, etc.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
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
        if size > 0 {
            size
        } else {
            8 * 1024 * 1024 * 1024
        } // fallback 8GB
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
                    free = line
                        .split(':')
                        .nth(1)
                        .map(|s| s.trim().trim_end_matches('.').parse().unwrap_or(0))
                        .unwrap_or(0);
                } else if line.starts_with("Pages inactive:") {
                    inactive = line
                        .split(':')
                        .nth(1)
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
                    let kb: u64 = line
                        .split_whitespace()
                        .nth(1)
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
    async fn launch(chrome_path: &str) -> Result<Self> {
        can_launch_chrome()?;

        let tmp_dir = tempfile::tempdir()
            .map_err(|e| anyhow!("failed to create temp dir for Chrome profile: {e}"))?;

        let mut child = tokio::process::Command::new(chrome_path)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-sandbox",
                "--disable-extensions",
                "--remote-debugging-port=0",
                "--window-size=1280,720",
            ])
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
        .map_err(|_| anyhow!("timed out waiting for Chrome DevTools URL"))??;

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
        let _ = self.child.start_kill();
        ACTIVE_INSTANCES.fetch_sub(1, Ordering::Relaxed);
        debug!(
            "Chrome instance dropped, active={}",
            ACTIVE_INSTANCES.load(Ordering::Relaxed)
        );
    }
}

// ---------------------------------------------------------------------------
// CdpClient -- WebSocket CDP transport
// ---------------------------------------------------------------------------

struct CdpClient {
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
            ws_tx,
            pending,
            events_rx: Mutex::new(events_rx),
            next_id: AtomicU32::new(1),
        })
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
    /// Last activity timestamp (for idle timeout).
    last_activity: Arc<AtomicU64>,
}

impl BrowserSession {
    /// Launch Chrome, discover the default page target, and connect CDP.
    pub async fn start(chrome_path: &str) -> Result<Self> {
        let chrome = ChromeProcess::launch(chrome_path).await?;
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
            last_activity: Arc::new(AtomicU64::new(now)),
        })
    }

    /// Connect CDP to a Chrome process's page target.
    async fn connect_cdp(chrome: &ChromeProcess) -> Result<CdpClient> {
        let port = chrome.port()?;
        let discovery_url = format!("http://127.0.0.1:{port}/json");
        let targets: Vec<Value> = reqwest::get(&discovery_url)
            .await
            .map_err(|e| anyhow!("failed to discover CDP targets: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("failed to parse CDP targets: {e}"))?;

        let page_target = targets
            .iter()
            .find(|t| t["type"].as_str() == Some("page"))
            .ok_or_else(|| anyhow!("no page target found in CDP target list"))?;

        let page_ws_url = page_target["webSocketDebuggerUrl"]
            .as_str()
            .ok_or_else(|| anyhow!("page target missing webSocketDebuggerUrl"))?;

        debug!(page_ws_url = %page_ws_url, "connecting to page target");

        let cdp = CdpClient::connect(page_ws_url).await?;
        cdp.send("Page.enable", json!({})).await?;
        cdp.send("DOM.enable", json!({})).await?;
        cdp.send("Runtime.enable", json!({})).await?;
        cdp.send("Network.enable", json!({})).await?;

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
        let new_chrome = ChromeProcess::launch(&self.chrome_path).await?;
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

        match action {
            "open" | "navigate" => self.cmd_open(args).await,
            "snapshot" => self.cmd_snapshot().await,
            "click" => self.cmd_click(args).await,
            "fill" | "type" => self.cmd_fill(args).await,
            "select" => self.cmd_select(args).await,
            "check" => self.cmd_check(args, true).await,
            "uncheck" => self.cmd_check(args, false).await,
            "scroll" => self.cmd_scroll(args).await,
            "screenshot" => self.cmd_screenshot().await,
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
            other => Err(anyhow!("web_browser: unsupported action `{other}`")),
        }
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

        let ref_count = parsed.get("refCount").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

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
        let eref = args
            .get("ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("click: `ref` required (e.g. @e3)"))?;

        // Use JS to find element by data-ref and click it.
        let js = format!(
            r#"(function(){{
                var el = document.querySelector('[data-ref="{}"]');
                if (!el) return 'NOT_FOUND';
                el.scrollIntoView({{block:'center'}});
                el.click();
                return 'OK';
            }})()"#,
            escape_js_string(eref)
        );

        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" {
            bail!("click: element {eref} not found (run snapshot first)");
        }

        // Small delay for any triggered navigation / DOM updates.
        time::sleep(Duration::from_millis(200)).await;

        Ok(json!({ "action": "click", "ref": eref, "text": format!("Clicked {eref}") }))
    }

    async fn cmd_fill(&self, args: &Value) -> Result<Value> {
        let eref = args.get("ref").and_then(|v| v.as_str());
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("fill/type: `text` required"))?;

        if let Some(eref) = eref {
            // Fill a specific element.
            let escaped_text = escape_js_string(text);
            let js = format!(
                r#"(function(){{
                    var el = document.querySelector('[data-ref="{}"]');
                    if (!el) return 'NOT_FOUND';
                    el.focus();
                    el.value = '{}';
                    el.dispatchEvent(new Event('input', {{bubbles:true}}));
                    el.dispatchEvent(new Event('change', {{bubbles:true}}));
                    return 'OK';
                }})()"#,
                escape_js_string(eref),
                escaped_text
            );

            let result = self.eval_js(&js).await?;
            if result == "NOT_FOUND" {
                bail!("fill: element {eref} not found (run snapshot first)");
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
            r#"(function(){{
                var el = document.querySelector('[data-ref="{}"]');
                if (!el) return 'NOT_FOUND';
                el.value = '{}';
                el.dispatchEvent(new Event('change', {{bubbles:true}}));
                return 'OK';
            }})()"#,
            escape_js_string(eref),
            escape_js_string(value)
        );

        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" {
            bail!("select: element {eref} not found");
        }

        Ok(
            json!({ "action": "select", "ref": eref, "text": format!("Selected {value} on {eref}") }),
        )
    }

    async fn cmd_check(&self, args: &Value, check: bool) -> Result<Value> {
        let eref = args
            .get("ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("check/uncheck: `ref` required"))?;

        let desired = if check { "true" } else { "false" };
        let js = format!(
            r#"(function(){{
                var el = document.querySelector('[data-ref="{}"]');
                if (!el) return 'NOT_FOUND';
                if (el.checked !== {}) el.click();
                return 'OK';
            }})()"#,
            escape_js_string(eref),
            desired
        );

        let result = self.eval_js(&js).await?;
        if result == "NOT_FOUND" {
            bail!("check: element {eref} not found");
        }

        let verb = if check { "Checked" } else { "Unchecked" };
        Ok(
            json!({ "action": if check { "check" } else { "uncheck" }, "ref": eref, "text": format!("{verb} {eref}") }),
        )
    }

    async fn cmd_scroll(&self, args: &Value) -> Result<Value> {
        let direction = args
            .get("direction")
            .and_then(|v| v.as_str())
            .unwrap_or("down");

        let delta = if direction == "up" { -500 } else { 500 };
        let js = format!("window.scrollBy(0, {delta})");
        self.eval_js(&js).await?;

        Ok(
            json!({ "action": "scroll", "direction": direction, "text": format!("Scrolled {direction}") }),
        )
    }

    async fn cmd_screenshot(&self) -> Result<Value> {
        let result = self
            .cdp
            .send("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;

        let data = result.get("data").and_then(|v| v.as_str()).unwrap_or("");

        Ok(json!({
            "action": "screenshot",
            "image": format!("data:image/png;base64,{data}")
        }))
    }

    async fn cmd_pdf(&self) -> Result<Value> {
        let result = self.cdp.send("Page.printToPDF", json!({})).await?;

        let data = result.get("data").and_then(|v| v.as_str()).unwrap_or("");

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
        let timeout_secs = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(15);

        let js = match target {
            "url" => format!(r#"location.href.includes('{}')"#, escape_js_string(value)),
            "text" => format!(
                r#"document.body.innerText.includes('{}')"#,
                escape_js_string(value)
            ),
            // Default: wait for an element matching a CSS selector.
            _ => format!(r#"!!document.querySelector('{}')"#, escape_js_string(value)),
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

        let value = result.get("result").and_then(|r| r.get("value"));

        match value {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(v) => Ok(v.to_string()),
            None => Ok(String::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Escape a string for embedding in a JS string literal (single-quoted).
fn escape_js_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

// ---------------------------------------------------------------------------
// Snapshot JS -- injected into the page to build an accessibility-like tree
// ---------------------------------------------------------------------------

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
    for (var child = node.firstChild; child; child = child.nextSibling) {
      walk(child, label ? depth + 1 : depth);
    }
  }
  if (document.body) walk(document.body, 0);
  return JSON.stringify({lines: lines, refCount: counter});
})()"#;
