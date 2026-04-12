# Web Browser Feature Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring rsclaw's web_browser tool to feature parity with agent-browser, covering headed mode, enhanced scroll/screenshot, keyboard press, wait enhancements, iframe support, multi-tab, auth persistence, viewport emulation, dialog handling, network interception, annotated screenshots, and more.

**Architecture:** All changes are in `src/browser/mod.rs` (CDP client) and `src/agent/runtime.rs` (tool definition). Each feature adds a new action or enhances an existing one via CDP protocol commands. No new crates needed — everything uses CDP over the existing WebSocket connection.

**Tech Stack:** Rust, Chrome DevTools Protocol (CDP), tokio async, serde_json

---

## Phase 1: Quick Wins (enhance existing actions)

### Task 1: Headed Mode

**Files:**
- Modify: `src/browser/mod.rs:157-177` (ChromeProcess::launch)
- Modify: `src/config/schema.rs:1339-1343` (ComputerUseConfig → WebBrowserConfig)

- [ ] **Step 1: Add `headed` field to WebBrowserConfig**

In `src/config/schema.rs`, add the field:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebBrowserConfig {
    pub enabled: Option<bool>,
    /// Path to Chrome/Chromium binary (auto-detect if not set)
    pub chrome_path: Option<String>,
    /// Run browser with visible window (default: false = headless)
    pub headed: Option<bool>,
}
```

- [ ] **Step 2: Pass headed flag into BrowserSession**

In `src/browser/mod.rs`, add `headed: bool` to `BrowserSession` struct and `new()`:

```rust
pub struct BrowserSession {
    chrome: ChromeProcess,
    cdp: CdpClient,
    refs: HashMap<String, String>,
    ref_counter: u32,
    chrome_path: String,
    headed: bool,
    last_activity: Arc<AtomicU64>,
}
```

Update `new()` to accept `headed: bool` and store it. Update `restart()` to pass it through.

- [ ] **Step 3: Conditionally remove --headless flag in launch**

In `ChromeProcess::launch`, accept a `headed: bool` parameter:

```rust
async fn launch(chrome_path: &str, headed: bool) -> Result<Self> {
    // ...
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
    let mut child = tokio::process::Command::new(chrome_path)
        .args(&args)
        // ...
```

- [ ] **Step 4: Wire config through runtime.rs**

Where `BrowserSession::new()` is called in `runtime.rs`, read `config.tools.web_browser.headed` and pass it through.

- [ ] **Step 5: Test headed mode manually**

Run rsclaw with `tools.webBrowser.headed: true` in config, verify Chrome window appears visibly.

- [ ] **Step 6: Commit**

```bash
git add src/browser/mod.rs src/config/schema.rs src/agent/runtime.rs
git commit -m "feat(browser): add headed mode for visible Chrome window"
```

---

### Task 2: Enhanced Scroll

**Files:**
- Modify: `src/browser/mod.rs:788-799` (cmd_scroll)
- Modify: `src/agent/runtime.rs` (tool definition)

- [ ] **Step 1: Enhance cmd_scroll to support distance, direction, and container**

Replace `cmd_scroll`:

```rust
async fn cmd_scroll(&self, args: &Value) -> Result<Value> {
    let direction = args.get("direction").and_then(|v| v.as_str()).unwrap_or("down");
    let amount = args.get("amount").and_then(|v| v.as_i64()).unwrap_or(500);
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
            r#"(function(){{
                var el = document.querySelector('{}');
                if (!el) return 'NOT_FOUND';
                el.scrollBy({dx}, {dy});
                return 'OK';
            }})()"#,
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
```

- [ ] **Step 2: Update tool definition parameters**

In `runtime.rs`, add `amount`, `selector` to the web_browser tool parameters, and update `direction` to include `left`/`right`.

- [ ] **Step 3: Test**: open a long page, scroll down 1000, scroll up 500, scroll in a container.

- [ ] **Step 4: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): enhanced scroll with distance, 4 directions, container selector"
```

---

### Task 3: Screenshot Enhancements (JPEG, quality, full page)

**Files:**
- Modify: `src/browser/mod.rs:801-816` (cmd_screenshot)

- [ ] **Step 1: Enhance cmd_screenshot**

```rust
async fn cmd_screenshot(&self, args: &Value) -> Result<Value> {
    let format = args.get("format").and_then(|v| v.as_str()).unwrap_or("png");
    let quality = args.get("quality").and_then(|v| v.as_i64());
    let full_page = args.get("full_page").and_then(|v| v.as_bool()).unwrap_or(false);

    // For full page: get page dimensions and set clip
    let mut params = json!({ "format": format });
    if let Some(q) = quality {
        params["quality"] = json!(q);
    }
    if full_page {
        // Get full page height
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

    let result = self.cdp.send("Page.captureScreenshot", params).await?;
    let data = result.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let mime = if format == "jpeg" { "image/jpeg" } else { "image/png" };

    Ok(json!({
        "action": "screenshot",
        "image": format!("data:{mime};base64,{data}")
    }))
}
```

- [ ] **Step 2: Update tool definition** — add `format`, `quality`, `full_page` params.

- [ ] **Step 3: Test**: screenshot as JPEG q50, full page screenshot.

- [ ] **Step 4: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): screenshot with JPEG/quality/full-page support"
```

---

### Task 4: Keyboard Press (Enter, Tab, Escape, etc.)

**Files:**
- Modify: `src/browser/mod.rs` (new cmd_press)
- Modify: `src/agent/runtime.rs` (dispatch + tool definition)

- [ ] **Step 1: Add cmd_press**

```rust
async fn cmd_press(&self, args: &Value) -> Result<Value> {
    let key = args.get("key").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("press: `key` required"))?;

    // Map key names to CDP key definitions
    let (key_code, code, text) = match key.to_lowercase().as_str() {
        "enter" | "return" => (13, "Enter", "\r"),
        "tab" => (9, "Tab", "\t"),
        "escape" | "esc" => (27, "Escape", ""),
        "backspace" => (8, "Backspace", ""),
        "delete" => (46, "Delete", ""),
        "arrowup" | "up" => (38, "ArrowUp", ""),
        "arrowdown" | "down" => (40, "ArrowDown", ""),
        "arrowleft" | "left" => (37, "ArrowLeft", ""),
        "arrowright" | "right" => (39, "ArrowRight", ""),
        "space" => (32, "Space", " "),
        "home" => (36, "Home", ""),
        "end" => (35, "End", ""),
        "pageup" => (33, "PageUp", ""),
        "pagedown" => (34, "PageDown", ""),
        _ => (0, key, ""),
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
```

- [ ] **Step 2: Add dispatch** in `execute()` match: `"press" => self.cmd_press(args).await,`

- [ ] **Step 3: Update tool definition** — add `press` to action enum, add `key` param description.

- [ ] **Step 4: Test**: open a form, fill text, press Enter.

- [ ] **Step 5: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): add press action for keyboard keys"
```

---

### Task 5: Wait Enhancements (networkidle, JS condition)

**Files:**
- Modify: `src/browser/mod.rs:871-911` (cmd_wait)

- [ ] **Step 1: Enhance cmd_wait with new target types**

Add two new target types to the match:

```rust
let js = match target {
    "url" => format!(r#"location.href.includes('{}')"#, escape_js_string(value)),
    "text" => format!(r#"document.body.innerText.includes('{}')"#, escape_js_string(value)),
    "networkidle" => {
        // Use Performance API: check if no network requests in last 500ms
        r#"(function(){
            var entries = performance.getEntriesByType('resource');
            if (entries.length === 0) return true;
            var last = entries[entries.length - 1];
            return (performance.now() - last.responseEnd) > 500;
        })()"#.to_string()
    }
    "fn" | "js" | "function" => {
        // User-provided JS expression that should return truthy
        value.to_string()
    }
    // Default: wait for CSS selector
    _ => format!(r#"!!document.querySelector('{}')"#, escape_js_string(value)),
};
```

- [ ] **Step 2: Update tool definition** — expand `target` description.

- [ ] **Step 3: Test**: `wait` with `target: "networkidle"`, `wait` with `target: "fn"` + custom JS.

- [ ] **Step 4: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): wait with networkidle and JS condition targets"
```

---

### Task 6: Viewport / Device Emulation

**Files:**
- Modify: `src/browser/mod.rs` (new cmd_set_viewport)

- [ ] **Step 1: Add cmd_set_viewport**

```rust
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
```

- [ ] **Step 2: Add dispatch**: `"set_viewport" => self.cmd_set_viewport(args).await,`

- [ ] **Step 3: Update tool definition** — add `set_viewport` to actions, add `width`, `height`, `scale`, `mobile` params.

- [ ] **Step 4: Test**: set viewport 375x812 (iPhone), take screenshot, verify narrow layout.

- [ ] **Step 5: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): viewport and device emulation via CDP"
```

---

### Task 7: Dialog Handling (alert/confirm/prompt)

**Files:**
- Modify: `src/browser/mod.rs` (new cmd_dialog + event listener)

- [ ] **Step 1: Track pending dialog state**

Add to `BrowserSession`:

```rust
pub struct BrowserSession {
    // ... existing fields ...
    pending_dialog: Option<String>, // Stores dialog message when one is pending
}
```

- [ ] **Step 2: Enable Page domain dialog events**

In `connect_cdp`, the `Page.enable` call already enables dialog events. Add a task that listens for `Page.javascriptDialogOpening` events and auto-accepts `alert` and `beforeunload`:

```rust
// After Page.enable, set up auto-accept for alert/beforeunload
cdp.send("Page.enable", json!({})).await?;
```

The dialog detection happens in `execute()` — before running any command, drain events and check for dialog:

```rust
// In execute(), before the action match:
{
    let mut rx = self.cdp.events_rx.lock().await;
    while let Ok(event) = rx.try_recv() {
        if event.get("method").and_then(|m| m.as_str()) == Some("Page.javascriptDialogOpening") {
            let msg = event["params"]["message"].as_str().unwrap_or("").to_string();
            let dtype = event["params"]["type"].as_str().unwrap_or("");
            // Auto-accept alert and beforeunload
            if dtype == "alert" || dtype == "beforeunload" {
                let _ = self.cdp.send("Page.handleJavaScriptDialog", json!({"accept": true})).await;
            } else {
                self.pending_dialog = Some(msg);
            }
        }
    }
}
```

- [ ] **Step 3: Add cmd_dialog**

```rust
async fn cmd_dialog(&mut self, args: &Value) -> Result<Value> {
    let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("accept");
    match sub {
        "accept" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let mut params = json!({"accept": true});
            if !text.is_empty() {
                params["promptText"] = json!(text);
            }
            self.cdp.send("Page.handleJavaScriptDialog", params).await?;
            self.pending_dialog = None;
            Ok(json!({"action": "dialog", "text": "Dialog accepted"}))
        }
        "dismiss" => {
            self.cdp.send("Page.handleJavaScriptDialog", json!({"accept": false})).await?;
            self.pending_dialog = None;
            Ok(json!({"action": "dialog", "text": "Dialog dismissed"}))
        }
        "status" => {
            Ok(json!({"action": "dialog", "pending": self.pending_dialog.is_some(),
                       "message": self.pending_dialog.as_deref().unwrap_or("")}))
        }
        _ => Err(anyhow!("dialog: unknown sub-action (use accept/dismiss/status)"))
    }
}
```

- [ ] **Step 4: Add dispatch + tool definition**

- [ ] **Step 5: Test**: navigate to page with `alert()`, verify auto-accept. Test `confirm()` with manual accept/dismiss.

- [ ] **Step 6: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): dialog handling with auto-accept for alert/beforeunload"
```

---

## Phase 2: Medium Complexity

### Task 8: Iframe Support

**Files:**
- Modify: `src/browser/mod.rs` (snapshot JS, frame context tracking)

- [ ] **Step 1: Enhance SNAPSHOT_JS to walk iframes**

Add iframe traversal to the snapshot JavaScript. When an `<iframe>` is encountered, access `iframe.contentDocument` (same-origin only) and walk it with indentation:

```javascript
// Inside walk() function, after the main element handling:
if (tag === 'iframe') {
    try {
        var iframeDoc = el.contentDocument || el.contentWindow.document;
        if (iframeDoc && iframeDoc.body) {
            lines.push('  '.repeat(depth + 1) + '[iframe-content]');
            walk(iframeDoc.body, depth + 2);
        }
    } catch(e) {
        lines.push('  '.repeat(depth + 1) + '[iframe: cross-origin, not accessible]');
    }
}
```

- [ ] **Step 2: Track frame context for refs**

When refs are inside iframes, the `data-ref` attribute is set on the iframe's document. The `cmd_click` / `cmd_fill` JS needs to also search inside iframe documents:

```javascript
// Updated element finder function used by click/fill/select/check:
function findRef(ref) {
    var el = document.querySelector('[data-ref="' + ref + '"]');
    if (el) return el;
    // Search inside same-origin iframes
    var iframes = document.querySelectorAll('iframe');
    for (var i = 0; i < iframes.length; i++) {
        try {
            var doc = iframes[i].contentDocument;
            if (doc) {
                el = doc.querySelector('[data-ref="' + ref + '"]');
                if (el) return el;
            }
        } catch(e) {}
    }
    return null;
}
```

- [ ] **Step 3: Update all action JS** to use `findRef()` instead of direct `document.querySelector('[data-ref=...]')` — affects `cmd_click`, `cmd_fill`, `cmd_select`, `cmd_check`.

- [ ] **Step 4: Test**: open a page with an iframe (e.g. embedded form), snapshot, click element inside iframe.

- [ ] **Step 5: Commit**

```bash
git add src/browser/mod.rs
git commit -m "feat(browser): iframe support in snapshot and element interactions"
```

---

### Task 9: Multi-Tab Support

**Files:**
- Modify: `src/browser/mod.rs` (tab management, target switching)

- [ ] **Step 1: Add tab management commands**

Add `cmd_new_tab`, `cmd_close_tab`, `cmd_switch_tab`, `cmd_list_tabs`:

```rust
async fn cmd_new_tab(&mut self, args: &Value) -> Result<Value> {
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

    // Activate the target
    self.cdp.send("Target.activateTarget", json!({"targetId": target_id})).await?;

    // Reconnect CDP to the new page target
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
    self.cdp.send("Target.closeTarget", json!({"targetId": target_id})).await?;
    Ok(json!({"action": "close_tab", "target_id": target_id}))
}
```

- [ ] **Step 2: Enable Target domain** in `connect_cdp`: `cdp.send("Target.setDiscoverTargets", json!({"discover": true})).await?;`

- [ ] **Step 3: Add dispatches**: `"new_tab" | "list_tabs" | "switch_tab" | "close_tab"`

- [ ] **Step 4: Update tool definition**

- [ ] **Step 5: Test**: open tab, list tabs, switch between them, close one.

- [ ] **Step 6: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): multi-tab support with create/list/switch/close"
```

---

### Task 10: Auth State Persistence (save/load cookies + localStorage)

**Files:**
- Modify: `src/browser/mod.rs` (new cmd_state_save, cmd_state_load)

- [ ] **Step 1: Add state save/load commands**

```rust
async fn cmd_state(&mut self, args: &Value) -> Result<Value> {
    let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("save");
    match sub {
        "save" => {
            // Get cookies
            let cookies_result = self.cdp.send("Network.getCookies", json!({})).await?;
            let cookies = cookies_result.get("cookies").cloned().unwrap_or(json!([]));

            // Get localStorage
            let local_storage = self.eval_js("JSON.stringify(Object.assign({}, localStorage))").await?;
            // Get sessionStorage
            let session_storage = self.eval_js("JSON.stringify(Object.assign({}, sessionStorage))").await?;
            // Get current URL
            let url = self.eval_js("location.href").await?;

            let state = json!({
                "url": url,
                "cookies": cookies,
                "localStorage": serde_json::from_str::<Value>(&local_storage).unwrap_or(json!({})),
                "sessionStorage": serde_json::from_str::<Value>(&session_storage).unwrap_or(json!({})),
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });

            Ok(json!({"action": "state", "sub": "save", "state": state}))
        }
        "load" => {
            let state = args.get("state")
                .ok_or_else(|| anyhow!("state load: `state` object required"))?;

            // Restore cookies
            if let Some(cookies) = state.get("cookies").and_then(|v| v.as_array()) {
                for cookie in cookies {
                    let _ = self.cdp.send("Network.setCookie", cookie.clone()).await;
                }
            }

            // Navigate to saved URL first (so localStorage domain matches)
            if let Some(url) = state.get("url").and_then(|v| v.as_str()) {
                self.cdp.send("Page.navigate", json!({"url": url})).await?;
                let _ = self.cdp.wait_event("Page.loadEventFired", 15).await;
            }

            // Restore localStorage
            if let Some(ls) = state.get("localStorage").and_then(|v| v.as_object()) {
                for (k, v) in ls {
                    let val = v.as_str().unwrap_or("");
                    self.eval_js(&format!(
                        "localStorage.setItem('{}', '{}')",
                        escape_js_string(k), escape_js_string(val)
                    )).await?;
                }
            }

            // Restore sessionStorage
            if let Some(ss) = state.get("sessionStorage").and_then(|v| v.as_object()) {
                for (k, v) in ss {
                    let val = v.as_str().unwrap_or("");
                    self.eval_js(&format!(
                        "sessionStorage.setItem('{}', '{}')",
                        escape_js_string(k), escape_js_string(val)
                    )).await?;
                }
            }

            // Reload to apply restored state
            self.cdp.send("Page.reload", json!({})).await?;
            let _ = self.cdp.wait_event("Page.loadEventFired", 15).await;

            Ok(json!({"action": "state", "sub": "load", "text": "State restored"}))
        }
        _ => Err(anyhow!("state: unknown sub-action (use save/load)"))
    }
}
```

- [ ] **Step 2: Add dispatch**: `"state" => self.cmd_state(args).await,`

- [ ] **Step 3: Update tool definition**

- [ ] **Step 4: Test**: login to a site, save state, restart browser, load state, verify still logged in.

- [ ] **Step 5: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): auth state persistence with save/load cookies+localStorage"
```

---

### Task 11: Network Interception

**Files:**
- Modify: `src/browser/mod.rs` (new cmd_network)

- [ ] **Step 1: Add network request tracking**

Add to `BrowserSession`:

```rust
pub struct BrowserSession {
    // ... existing ...
    network_requests: Vec<Value>,  // Tracked network requests
}
```

Enable `Network` domain request tracking in `connect_cdp` (already enabled).

- [ ] **Step 2: Add cmd_network**

```rust
async fn cmd_network(&mut self, args: &Value) -> Result<Value> {
    let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("requests");
    match sub {
        "requests" => {
            // Get recent network requests via Performance API
            let js = r#"JSON.stringify(
                performance.getEntriesByType('resource').slice(-50).map(e => ({
                    name: e.name, type: e.initiatorType,
                    duration: Math.round(e.duration),
                    size: e.transferSize || 0
                }))
            )"#;
            let result = self.eval_js(js).await?;
            let entries: Value = serde_json::from_str(&result).unwrap_or(json!([]));
            Ok(json!({"action": "network", "requests": entries}))
        }
        "block" => {
            // Block URLs matching a pattern
            let pattern = args.get("pattern").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("network block: `pattern` required"))?;
            self.cdp.send("Network.setBlockedURLs", json!({"urls": [pattern]})).await?;
            Ok(json!({"action": "network", "text": format!("Blocking {pattern}")}))
        }
        "unblock" => {
            self.cdp.send("Network.setBlockedURLs", json!({"urls": []})).await?;
            Ok(json!({"action": "network", "text": "All URL blocks removed"}))
        }
        _ => Err(anyhow!("network: unknown sub-action (use requests/block/unblock)"))
    }
}
```

- [ ] **Step 3: Add dispatch + tool definition**

- [ ] **Step 4: Test**: list requests, block a domain, verify blocked.

- [ ] **Step 5: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): network request tracking and URL blocking"
```

---

### Task 12: Annotated Screenshot

**Files:**
- Modify: `src/browser/mod.rs` (enhance cmd_screenshot)

- [ ] **Step 1: Add annotation option to screenshot**

When `annotate: true`, inject CSS/JS to overlay numbered labels on interactive elements before capturing:

```rust
// Inside cmd_screenshot, before capture:
if args.get("annotate").and_then(|v| v.as_bool()).unwrap_or(false) {
    // First do a snapshot to get refs
    self.cmd_snapshot().await?;

    // Inject annotation overlay
    let annotate_js = r#"(function(){
        var refs = document.querySelectorAll('[data-ref]');
        var labels = [];
        refs.forEach(function(el) {
            var ref = el.getAttribute('data-ref');
            var num = ref.replace('@e', '');
            var rect = el.getBoundingClientRect();
            var label = document.createElement('div');
            label.className = '__rsclaw_annotation';
            label.textContent = num;
            label.style.cssText = 'position:fixed;z-index:999999;background:red;color:white;' +
                'font-size:11px;font-weight:bold;padding:1px 4px;border-radius:8px;' +
                'pointer-events:none;left:' + (rect.left-4) + 'px;top:' + (rect.top-4) + 'px;';
            document.body.appendChild(label);
            labels.push({num: parseInt(num), ref: ref, tag: el.tagName.toLowerCase(),
                text: (el.innerText||el.value||el.alt||'').substring(0, 50)});
        });
        return JSON.stringify(labels);
    })()"#;

    let legend_raw = self.eval_js(annotate_js).await?;

    // Take screenshot with annotations
    let result = self.cdp.send("Page.captureScreenshot", params).await?;
    let data = result.get("data").and_then(|v| v.as_str()).unwrap_or("");

    // Remove annotations
    self.eval_js("document.querySelectorAll('.__rsclaw_annotation').forEach(e => e.remove())").await?;

    let legend: Value = serde_json::from_str(&legend_raw).unwrap_or(json!([]));

    return Ok(json!({
        "action": "screenshot",
        "image": format!("data:{mime};base64,{data}"),
        "legend": legend,
    }));
}
```

- [ ] **Step 2: Update tool definition** — add `annotate` boolean param.

- [ ] **Step 3: Test**: annotated screenshot, verify numbered labels in image and legend in response.

- [ ] **Step 4: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): annotated screenshots with numbered element labels"
```

---

## Phase 3: Small Additions

### Task 13: Remaining Small Features (batch)

**Files:**
- Modify: `src/browser/mod.rs`
- Modify: `src/agent/runtime.rs`

- [ ] **Step 1: Add `highlight` action**

```rust
async fn cmd_highlight(&self, args: &Value) -> Result<Value> {
    let eref = args.get("ref").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("highlight: `ref` required"))?;
    let js = format!(
        r#"(function(){{
            var el = document.querySelector('[data-ref="{}"]');
            if (!el) return 'NOT_FOUND';
            el.style.outline = '3px solid red';
            el.style.outlineOffset = '2px';
            el.scrollIntoView({{block:'center'}});
            return 'OK';
        }})()"#,
        escape_js_string(eref)
    );
    let result = self.eval_js(&js).await?;
    if result == "NOT_FOUND" { bail!("highlight: {eref} not found"); }
    Ok(json!({"action": "highlight", "ref": eref}))
}
```

- [ ] **Step 2: Add `clipboard` action**

```rust
async fn cmd_clipboard(&self, args: &Value) -> Result<Value> {
    let sub = args.get("value").and_then(|v| v.as_str()).unwrap_or("read");
    match sub {
        "read" => {
            let text = self.eval_js("navigator.clipboard.readText()").await
                .unwrap_or_default();
            Ok(json!({"action": "clipboard", "text": text}))
        }
        "write" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            self.eval_js(&format!(
                "navigator.clipboard.writeText('{}')", escape_js_string(text)
            )).await?;
            Ok(json!({"action": "clipboard", "text": "Written to clipboard"}))
        }
        _ => Err(anyhow!("clipboard: use read/write"))
    }
}
```

- [ ] **Step 3: Add `find` action (semantic locator)**

```rust
async fn cmd_find(&self, args: &Value) -> Result<Value> {
    let by = args.get("by").and_then(|v| v.as_str()).unwrap_or("text");
    let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let then = args.get("then").and_then(|v| v.as_str());

    let js = match by {
        "text" => format!(
            r#"(function(){{
                var all = document.querySelectorAll('a, button, [role=button], [role=link]');
                for (var i = 0; i < all.length; i++) {{
                    if (all[i].innerText && all[i].innerText.trim().includes('{}')) {{
                        all[i].scrollIntoView({{block:'center'}});
                        return JSON.stringify({{found: true, tag: all[i].tagName, text: all[i].innerText.substring(0,100)}});
                    }}
                }}
                return JSON.stringify({{found: false}});
            }})()"#,
            escape_js_string(value)
        ),
        "label" => format!(
            r#"(function(){{
                var labels = document.querySelectorAll('label');
                for (var i = 0; i < labels.length; i++) {{
                    if (labels[i].textContent.includes('{}')) {{
                        var input = labels[i].querySelector('input, select, textarea')
                            || document.getElementById(labels[i].getAttribute('for'));
                        if (input) {{
                            input.scrollIntoView({{block:'center'}});
                            return JSON.stringify({{found: true, tag: input.tagName}});
                        }}
                    }}
                }}
                return JSON.stringify({{found: false}});
            }})()"#,
            escape_js_string(value)
        ),
        _ => return Err(anyhow!("find: `by` must be text or label")),
    };

    let result_str = self.eval_js(&js).await?;
    let result: Value = serde_json::from_str(&result_str).unwrap_or(json!({"found": false}));

    if result["found"].as_bool() == Some(true) {
        // If `then` is specified, perform the action
        if let Some(action) = then {
            match action {
                "click" => {
                    // The element is already scrolled into view; click it
                    let click_js = match by {
                        "text" => format!(
                            r#"(function(){{
                                var all = document.querySelectorAll('a, button, [role=button], [role=link]');
                                for (var i = 0; i < all.length; i++) {{
                                    if (all[i].innerText && all[i].innerText.trim().includes('{}')) {{
                                        all[i].click(); return 'OK';
                                    }}
                                }}
                                return 'NOT_FOUND';
                            }})()"#,
                            escape_js_string(value)
                        ),
                        _ => return Err(anyhow!("find+click only supported with by=text")),
                    };
                    self.eval_js(&click_js).await?;
                }
                _ => {}
            }
        }
    }

    Ok(json!({"action": "find", "by": by, "value": value, "result": result}))
}
```

- [ ] **Step 4: Add all dispatches**

```rust
"highlight" => self.cmd_highlight(args).await,
"clipboard" => self.cmd_clipboard(args).await,
"find" => self.cmd_find(args).await,
```

- [ ] **Step 5: Update tool definition with all new actions**

- [ ] **Step 6: Test each**: highlight an element, clipboard read/write, find text + click.

- [ ] **Step 7: Commit**

```bash
git add src/browser/mod.rs src/agent/runtime.rs
git commit -m "feat(browser): add highlight, clipboard, and semantic find actions"
```

---

### Task 14: Update Tool Definition (final)

**Files:**
- Modify: `src/agent/runtime.rs` (tool definition)

- [ ] **Step 1: Update the web_browser ToolDef with all new actions and parameters**

```rust
tools.push(ToolDef {
    name: "web_browser".to_owned(),
    description: "Control a web browser via CDP. Actions: open, snapshot, click, fill, type, \
        select, check, uncheck, scroll, screenshot, pdf, back, forward, reload, \
        get_text, get_url, get_title, wait, evaluate, cookies, press, set_viewport, \
        dialog, state, network, new_tab, list_tabs, switch_tab, close_tab, \
        highlight, clipboard, find".to_owned(),
    parameters: json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": [
                "open", "navigate", "snapshot", "click", "fill", "type",
                "select", "check", "uncheck", "scroll", "screenshot", "pdf",
                "back", "forward", "reload", "get_text", "get_url", "get_title",
                "wait", "evaluate", "cookies", "press", "set_viewport",
                "dialog", "state", "network", "new_tab", "list_tabs",
                "switch_tab", "close_tab", "highlight", "clipboard", "find"
            ]},
            "url":        {"type": "string", "description": "URL for open/navigate"},
            "ref":        {"type": "string", "description": "Element ref like @e3 from snapshot"},
            "text":       {"type": "string", "description": "Text for fill/type/clipboard write/dialog prompt"},
            "value":      {"type": "string", "description": "Value for select, or sub-action for cookies/state/dialog/network/clipboard"},
            "key":        {"type": "string", "description": "Key name for press (Enter, Tab, Escape, etc.)"},
            "direction":  {"type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction"},
            "amount":     {"type": "integer", "description": "Scroll distance in pixels (default 500)"},
            "selector":   {"type": "string", "description": "CSS selector for scroll container"},
            "js":         {"type": "string", "description": "JavaScript for evaluate action"},
            "target":     {"type": "string", "description": "Wait target: element (CSS selector), text, url, networkidle, fn"},
            "timeout":    {"type": "number", "description": "Timeout in seconds (default 15)"},
            "format":     {"type": "string", "enum": ["png", "jpeg"], "description": "Screenshot format"},
            "quality":    {"type": "integer", "description": "JPEG quality (1-100)"},
            "full_page":  {"type": "boolean", "description": "Capture full scrollable page"},
            "annotate":   {"type": "boolean", "description": "Overlay numbered labels on interactive elements"},
            "width":      {"type": "integer", "description": "Viewport width for set_viewport"},
            "height":     {"type": "integer", "description": "Viewport height for set_viewport"},
            "scale":      {"type": "number", "description": "Device scale factor for set_viewport"},
            "mobile":     {"type": "boolean", "description": "Mobile emulation for set_viewport"},
            "target_id":  {"type": "string", "description": "Tab target ID for switch_tab/close_tab"},
            "state":      {"type": "object", "description": "State object for state load"},
            "pattern":    {"type": "string", "description": "URL pattern for network block"},
            "by":         {"type": "string", "enum": ["text", "label"], "description": "Find element by text or label"},
            "then":       {"type": "string", "description": "Action after find (click)"},
            "cookie":     {"type": "object", "description": "Cookie object for cookies set"}
        },
        "required": ["action"]
    }),
});
```

- [ ] **Step 2: Verify compilation**

```bash
cargo check 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git add src/agent/runtime.rs
git commit -m "feat(browser): update tool definition with all 33 actions"
```

---

## Summary

| Phase | Tasks | New Actions |
|-------|-------|-------------|
| Phase 1 (Quick Wins) | Tasks 1-7 | headed mode, enhanced scroll, screenshot JPEG/full-page, press, wait networkidle/fn, set_viewport, dialog |
| Phase 2 (Medium) | Tasks 8-12 | iframe support, multi-tab (4 actions), state save/load, network requests/block, annotated screenshot |
| Phase 3 (Small) | Tasks 13-14 | highlight, clipboard, semantic find, final tool definition |

**Total: 33 actions** (from current 20), covering all agent-browser features except video recording, profiler, and cloud providers (which are external tool concerns, not core browser actions).
