# Jimeng WASM Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a WASM plugin runtime for rsclaw and a jimeng.wasm plugin that automates Jimeng AI platform (image generation first, then video, then digital human).

**Architecture:** rsclaw loads `.wasm` files from `~/.rsclaw/plugins/` via wasmtime. Each plugin exports `get_manifest()` and `handle_tool()`. The host provides browser automation functions as WASM imports. The jimeng plugin is a separate Rust crate compiled to `wasm32-wasip2`.

**Tech Stack:** Rust 2024, wasmtime 29+, wit-bindgen, wasm32-wasip2 target, rsclaw browser module (CDP)

---

## File Structure

### rsclaw (host side) — `~/dev/rsclaw-jimeng/`

| File | Responsibility |
|------|---------------|
| `src/plugin/wasm_runtime.rs` | NEW: wasmtime engine, host function registration, WASM plugin loading |
| `src/plugin/mod.rs` | MODIFY: add `wasm_runtime` module, integrate WASM plugins into PluginRegistry |
| `src/plugin/manifest.rs` | MODIFY: extend scan_plugins to detect `.wasm` files |
| `src/agent/runtime.rs` | MODIFY: add WASM plugin tool dispatch (between MCP and Skill in chain) |
| `src/agent/tools_builder.rs` | MODIFY: register WASM plugin tools in LLM tool list |
| `Cargo.toml` | MODIFY: add wasmtime dependency |
| `tests/wasm_plugin_test.rs` | NEW: integration test for WASM plugin loading + tool dispatch |

### jimeng plugin — `~/dev/rsclaw-plugins/jimeng/`

| File | Responsibility |
|------|---------------|
| `Cargo.toml` | Crate config, wasm32-wasip2 target |
| `wit/world.wit` | WIT interface definition (host imports + plugin exports) |
| `src/lib.rs` | Entry: get_manifest(), handle_tool() dispatch |
| `src/manifest.rs` | Tool definitions (name, description, parameters JSON) |
| `src/host.rs` | Host function bindings (browser_*, log, sleep) |
| `src/jimeng/mod.rs` | Tool name router |
| `src/jimeng/auth.rs` | Login detection, QR code screenshot flow |
| `src/jimeng/common.rs` | Shared helpers: wait_for_generation, select_model, find_and_click_image |
| `src/jimeng/txt2img.rs` | Text-to-image automation |
| `src/jimeng/img2img.rs` | Image-to-image automation |
| `src/jimeng/txt2vid.rs` | Text-to-video automation |
| `src/jimeng/img2vid.rs` | Image-to-video automation |
| `src/jimeng/digital_human.rs` | Digital human create/talk/clone |

---

## Task 1: Add wasmtime dependency to rsclaw

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add wasmtime to Cargo.toml**

```toml
# Under [dependencies], add:
wasmtime = { version = "29", features = ["async"] }
```

- [ ] **Step 2: Verify it compiles**

Run: `cd ~/dev/rsclaw-jimeng && RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo check 2>&1 | tail -5`
Expected: `Finished` with no new errors

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add wasmtime for WASM plugin runtime"
```

---

## Task 2: Define WIT interface

**Files:**
- Create: `~/dev/rsclaw-plugins/jimeng/wit/world.wit`

- [ ] **Step 1: Create the WIT world definition**

```wit
// wit/world.wit
package rsclaw:jimeng;

/// Host functions provided by rsclaw to the plugin.
interface host-browser {
    /// Navigate to URL. Returns page title or error.
    browser-open: func(url: string) -> result<string, string>;
    /// Get accessibility snapshot as JSON string.
    browser-snapshot: func() -> result<string, string>;
    /// Click element by ref string (e.g. "@e5").
    browser-click: func(ref-str: string) -> result<string, string>;
    /// Click at pixel coordinates.
    browser-click-at: func(x: u32, y: u32) -> result<string, string>;
    /// Clear field and type text.
    browser-fill: func(ref-str: string, text: string) -> result<string, string>;
    /// Press a keyboard key.
    browser-press: func(key: string) -> result<string, string>;
    /// Scroll page.
    browser-scroll: func(direction: string, amount: u32) -> result<string, string>;
    /// Execute JavaScript, return result.
    browser-eval: func(code: string) -> result<string, string>;
    /// Wait for text to appear on page.
    browser-wait-text: func(text: string, timeout-ms: u32) -> result<string, string>;
    /// Take screenshot, return base64 PNG.
    browser-screenshot: func() -> result<string, string>;
    /// Download by clicking ref, save to filename. Returns path.
    browser-download: func(ref-str: string, filename: string) -> result<string, string>;
    /// Upload file to input ref.
    browser-upload: func(ref-str: string, filepath: string) -> result<string, string>;
    /// Get current page URL.
    browser-get-url: func() -> result<string, string>;
}

interface host-runtime {
    /// Log message. level: "info"|"warn"|"error"|"debug".
    log: func(level: string, msg: string);
    /// Sleep milliseconds (host enforces minimum 1000ms).
    sleep: func(ms: u32);
    /// Read file contents as UTF-8 string.
    read-file: func(path: string) -> result<string, string>;
}

/// Functions the plugin exports to rsclaw.
interface plugin-api {
    /// Return plugin manifest as JSON string.
    /// JSON shape: { name, version, description, tools: [{name, description, parameters}] }
    get-manifest: func() -> string;
    /// Handle a tool call. Returns result JSON or error.
    handle-tool: func(tool-name: string, args-json: string) -> result<string, string>;
}

world jimeng-plugin {
    import host-browser;
    import host-runtime;
    export plugin-api;
}
```

- [ ] **Step 2: Commit**

```bash
cd ~/dev/rsclaw-plugins
git add jimeng/wit/world.wit
git commit -m "feat(jimeng): add WIT interface definition"
```

---

## Task 3: Create jimeng plugin scaffold

**Files:**
- Create: `~/dev/rsclaw-plugins/jimeng/Cargo.toml`
- Create: `~/dev/rsclaw-plugins/jimeng/src/lib.rs`
- Create: `~/dev/rsclaw-plugins/jimeng/src/host.rs`
- Create: `~/dev/rsclaw-plugins/jimeng/src/manifest.rs`

- [ ] **Step 1: Create Cargo.toml**

```toml
[package]
name = "rsclaw-plugin-jimeng"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
wit-bindgen = "0.41"

[profile.release]
opt-level = "s"
lto = true
strip = true
```

- [ ] **Step 2: Create src/lib.rs — plugin entry point**

```rust
//! Jimeng AI platform automation plugin for rsclaw.

mod host;
mod manifest;
mod jimeng;

wit_bindgen::generate!({
    world: "jimeng-plugin",
    path: "wit",
});

struct JimengPlugin;

impl Guest for JimengPlugin {
    fn get_manifest() -> String {
        manifest::manifest_json()
    }

    fn handle_tool(tool_name: String, args_json: String) -> Result<String, String> {
        jimeng::dispatch(&tool_name, &args_json)
    }
}

export!(JimengPlugin);
```

- [ ] **Step 3: Create src/host.rs — host function wrappers**

```rust
//! Safe wrappers around host-provided functions.

use crate::rsclaw::jimeng::host_browser;
use crate::rsclaw::jimeng::host_runtime;

/// Navigate to URL. Returns page title.
pub fn browser_open(url: &str) -> Result<String, String> {
    host_browser::browser_open(url)
}

/// Get page snapshot as JSON.
pub fn browser_snapshot() -> Result<String, String> {
    host_browser::browser_snapshot()
}

/// Click element by ref.
pub fn browser_click(ref_str: &str) -> Result<String, String> {
    host_browser::browser_click(ref_str)
}

/// Click at coordinates.
pub fn browser_click_at(x: u32, y: u32) -> Result<String, String> {
    host_browser::browser_click_at(x, y)
}

/// Fill text into element.
pub fn browser_fill(ref_str: &str, text: &str) -> Result<String, String> {
    host_browser::browser_fill(ref_str, text)
}

/// Press keyboard key.
pub fn browser_press(key: &str) -> Result<String, String> {
    host_browser::browser_press(key)
}

/// Scroll page.
pub fn browser_scroll(direction: &str, amount: u32) -> Result<String, String> {
    host_browser::browser_scroll(direction, amount)
}

/// Execute JavaScript.
pub fn browser_eval(code: &str) -> Result<String, String> {
    host_browser::browser_eval(code)
}

/// Wait for text to appear.
pub fn browser_wait_text(text: &str, timeout_ms: u32) -> Result<String, String> {
    host_browser::browser_wait_text(text, timeout_ms)
}

/// Take screenshot.
pub fn browser_screenshot() -> Result<String, String> {
    host_browser::browser_screenshot()
}

/// Download file.
pub fn browser_download(ref_str: &str, filename: &str) -> Result<String, String> {
    host_browser::browser_download(ref_str, filename)
}

/// Upload file.
pub fn browser_upload(ref_str: &str, filepath: &str) -> Result<String, String> {
    host_browser::browser_upload(ref_str, filepath)
}

/// Get current URL.
pub fn browser_get_url() -> Result<String, String> {
    host_browser::browser_get_url()
}

/// Log message.
pub fn log(level: &str, msg: &str) {
    host_runtime::log(level, msg);
}

/// Sleep with rate limiting.
pub fn sleep(ms: u32) {
    host_runtime::sleep(ms);
}

/// Read file.
pub fn read_file(path: &str) -> Result<String, String> {
    host_runtime::read_file(path)
}
```

- [ ] **Step 4: Create src/manifest.rs — tool definitions**

```rust
//! Plugin manifest with all tool definitions.

use serde_json::json;

pub fn manifest_json() -> String {
    let manifest = json!({
        "name": "jimeng",
        "version": "0.1.0",
        "description": "Jimeng AI creative platform automation (text-to-image, video, digital human)",
        "disclaimer": "Personal use only. Users must use their own Jimeng account.",
        "tools": tools()
    });
    manifest.to_string()
}

fn tools() -> serde_json::Value {
    json!([
        // -- Image Generation --
        {
            "name": "txt2img",
            "description": "Generate image from text description using Jimeng Seedream models",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Image description (Chinese or English)"},
                    "model": {"type": "string", "description": "Model version: 5.0Lite|4.6|4.5|4.1|4.0|3.1|3.0", "default": "4.6"},
                    "ratio": {"type": "string", "description": "Aspect ratio: 1:1|16:9|9:16|4:3|3:4", "default": "1:1"},
                    "resolution": {"type": "string", "description": "Resolution: 2K|4K", "default": "2K"}
                },
                "required": ["prompt"]
            }
        },
        {
            "name": "img2img",
            "description": "Transform image with text instructions",
            "parameters": {
                "type": "object",
                "properties": {
                    "image_path": {"type": "string", "description": "Path to source image"},
                    "prompt": {"type": "string", "description": "Transformation instructions"}
                },
                "required": ["image_path", "prompt"]
            }
        },
        // -- Video Generation --
        {
            "name": "txt2vid",
            "description": "Generate video from text description using Jimeng Seedance models",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Video description"},
                    "duration": {"type": "integer", "description": "Duration in seconds (4-15)", "default": 5},
                    "resolution": {"type": "string", "description": "720P|1080P", "default": "720P"}
                },
                "required": ["prompt"]
            }
        },
        {
            "name": "img2vid",
            "description": "Animate a static image into video",
            "parameters": {
                "type": "object",
                "properties": {
                    "image_path": {"type": "string", "description": "Path to source image"},
                    "prompt": {"type": "string", "description": "Motion/animation description"}
                },
                "required": ["image_path"]
            }
        },
        // -- Digital Human --
        {
            "name": "digital_human_talk",
            "description": "Generate talking-head video from portrait + text/audio",
            "parameters": {
                "type": "object",
                "properties": {
                    "image_path": {"type": "string", "description": "Portrait image path"},
                    "text": {"type": "string", "description": "Speech text (if no audio)"},
                    "audio_path": {"type": "string", "description": "Audio file path (overrides text)"},
                    "voice": {"type": "string", "description": "Voice style name"}
                },
                "required": ["image_path"]
            }
        }
    ])
}
```

- [ ] **Step 5: Create src/jimeng/mod.rs — tool router**

```rust
//! Tool dispatch router.

pub mod auth;
pub mod common;
pub mod txt2img;

/// Dispatch a tool call to the appropriate handler.
pub fn dispatch(tool_name: &str, args_json: &str) -> Result<String, String> {
    let args: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| format!("invalid args JSON: {e}"))?;

    // Rate limit: ensure minimum interval between actions
    crate::host::sleep(2000);

    // Check login state
    auth::ensure_logged_in()?;

    match tool_name {
        "txt2img" => txt2img::run(&args),
        "img2img" => Err("img2img: not yet implemented".into()),
        "txt2vid" => Err("txt2vid: not yet implemented".into()),
        "img2vid" => Err("img2vid: not yet implemented".into()),
        "digital_human_talk" => Err("digital_human_talk: not yet implemented".into()),
        other => Err(format!("unknown tool: {other}")),
    }
}
```

- [ ] **Step 6: Create src/jimeng/auth.rs — login detection**

```rust
//! Login detection and QR code flow.

use crate::host;

/// Check if user is logged in to Jimeng. If not, screenshot QR and return error.
pub fn ensure_logged_in() -> Result<(), String> {
    let url = host::browser_get_url()?;

    // If not on jimeng, navigate first
    if !url.contains("jimeng.jianying.com") {
        host::browser_open("https://jimeng.jianying.com/ai-tool/generate/?type=image")?;
        host::sleep(5000);
    }

    // Check for login indicators in snapshot
    let snap = host::browser_snapshot()?;
    if snap.contains("登录") && !snap.contains("基础会员") && !snap.contains("VIP") {
        // Not logged in — screenshot for user
        host::log("warn", "Jimeng login required. Please scan QR code.");
        let _screenshot = host::browser_screenshot()?;
        return Err("Please log in to Jimeng first. A screenshot of the login page has been taken.".into());
    }

    Ok(())
}
```

- [ ] **Step 7: Create src/jimeng/common.rs — shared helpers**

```rust
//! Shared automation helpers.

use crate::host;

/// Select a model from the dropdown.
/// Models: "5.0 Lite", "4.6", "4.5", "4.1", "4.0", "3.1", "3.0"
pub fn select_model(model: &str) -> Result<(), String> {
    // Open model dropdown via JS click on the select element
    host::browser_eval(
        r#"(function(){var els=document.querySelectorAll('[class*=select]');for(var e of els){if(e.innerText&&e.innerText.match(/图片\s*\d/)&&e.offsetHeight>0&&e.offsetHeight<50){e.click();return 'ok'}}return 'miss'})()"#
    )?;
    host::sleep(2000);

    // Find and click the matching model option
    let snap = host::browser_snapshot()?;
    let target = match model {
        "5.0Lite" | "5.0 Lite" => "5.0 Lite",
        "4.6" => "4.6",
        "4.5" => "4.5",
        "4.1" => "4.1",
        "4.0" => "4.0",
        "3.1" => "3.1",
        "3.0" => "3.0",
        _ => "4.6", // default
    };

    // Parse snapshot to find option ref containing the target model
    if let Some(ref_str) = find_ref_containing(&snap, &format!("图片{}", target))
        .or_else(|| find_ref_containing(&snap, &format!("图片 {}", target)))
    {
        host::browser_click(&ref_str)?;
        host::sleep(2000);
    } else {
        host::log("warn", &format!("model {} not found in dropdown, using default", target));
        // Close dropdown by pressing Escape
        host::browser_press("Escape")?;
        host::sleep(1000);
    }

    Ok(())
}

/// Wait for image generation to complete.
/// Polls snapshot for "生成完成" text, up to timeout_ms.
pub fn wait_for_generation(timeout_ms: u32) -> Result<(), String> {
    let interval = 5000u32;
    let mut elapsed = 0u32;

    while elapsed < timeout_ms {
        host::sleep(interval);
        elapsed += interval;

        let snap = host::browser_snapshot()?;
        if snap.contains("生成完成") {
            host::log("info", &format!("generation completed after {}ms", elapsed));
            return Ok(());
        }
        if snap.contains("造梦中") || snap.contains("生成中") {
            host::log("info", &format!("generating... {}ms", elapsed));
            continue;
        }
    }

    Err(format!("generation timed out after {}ms", timeout_ms))
}

/// Click the first generated image using JS dispatchEvent (reliable click).
pub fn click_first_generated_image() -> Result<(), String> {
    host::browser_eval(
        r#"(function(){var imgs=document.querySelectorAll('img');for(var img of imgs){var src=img.src||'';if(src.indexOf('dreamina-sign')>-1&&img.offsetHeight>80){var rect=img.getBoundingClientRect();if(rect.y>0&&rect.y<600){['mousedown','mouseup','click'].forEach(function(t){img.dispatchEvent(new MouseEvent(t,{bubbles:true,cancelable:true,view:window,clientX:rect.x+rect.width/2,clientY:rect.y+rect.height/2,button:0}))});return 'clicked'}}}return 'none'})()"#
    )?;
    host::sleep(3000);
    Ok(())
}

/// Find a ref string (e.g. "@e5") in snapshot text that contains the given substring.
pub fn find_ref_containing(snapshot: &str, needle: &str) -> Option<String> {
    for line in snapshot.lines() {
        if line.contains(needle) {
            // Extract ref=eNN pattern
            if let Some(start) = line.find("ref=e") {
                let rest = &line[start + 4..]; // skip "ref="
                let end = rest.find(|c: char| c == ']' || c == ' ' || c == '"').unwrap_or(rest.len());
                let ref_id = &rest[..end];
                return Some(format!("@{}", ref_id));
            }
        }
    }
    None
}

/// Find the download button ref in current snapshot.
pub fn find_download_button() -> Result<String, String> {
    let snap = host::browser_snapshot()?;
    find_ref_containing(&snap, "下载")
        .ok_or_else(|| "download button not found in snapshot".into())
}
```

- [ ] **Step 8: Create src/jimeng/txt2img.rs — text-to-image**

```rust
//! Text-to-image generation via Jimeng web automation.

use crate::host;
use super::common;

/// Generate an image from text prompt.
///
/// Flow:
/// 1. Navigate to image generation page
/// 2. Select model
/// 3. Find input textbox, fill prompt
/// 4. Press Enter to generate
/// 5. Wait for generation to complete (up to 60s)
/// 6. Click first result image
/// 7. Download image
/// 8. Return file path
pub fn run(args: &serde_json::Value) -> Result<String, String> {
    let prompt = args.get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("txt2img: `prompt` required")?;
    let model = args.get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("4.6");
    let _ratio = args.get("ratio")
        .and_then(|v| v.as_str())
        .unwrap_or("1:1");
    let _resolution = args.get("resolution")
        .and_then(|v| v.as_str())
        .unwrap_or("2K");

    host::log("info", &format!("txt2img: prompt='{}', model={}", &prompt[..prompt.len().min(50)], model));

    // 1. Navigate to image generation page
    host::browser_open("https://jimeng.jianying.com/ai-tool/generate/?type=image")?;
    host::sleep(5000);

    // 2. Select model
    common::select_model(model)?;

    // 3. Find textbox and fill prompt
    let snap = host::browser_snapshot()?;
    let textbox_ref = common::find_ref_containing(&snap, "textbox")
        .or_else(|| common::find_ref_containing(&snap, "上传参考图"))
        .ok_or("txt2img: could not find input textbox")?;

    host::browser_fill(&textbox_ref, prompt)?;
    host::sleep(2000);

    // 4. Press Enter to generate
    host::browser_press("Enter")?;
    host::log("info", "txt2img: generation started");

    // 5. Wait for generation (up to 60 seconds)
    common::wait_for_generation(60000)?;

    // 6. Click first generated image
    common::click_first_generated_image()?;

    // 7. Find and click download button
    let dl_ref = common::find_download_button()?;
    let filename = format!("jimeng_txt2img_{}.png", chrono_timestamp());
    let path = host::browser_download(&dl_ref, &filename)?;

    // 8. Close preview
    host::browser_press("Escape")?;
    host::sleep(1000);

    host::log("info", &format!("txt2img: saved to {}", path));

    let result = serde_json::json!({
        "status": "ok",
        "path": path,
        "prompt": prompt,
        "model": model,
    });
    Ok(result.to_string())
}

fn chrono_timestamp() -> String {
    // Simple timestamp without chrono dependency
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{}", now)
}
```

- [ ] **Step 9: Commit plugin scaffold**

```bash
cd ~/dev/rsclaw-plugins
git add jimeng/
git commit -m "feat(jimeng): plugin scaffold with txt2img implementation

- WIT interface (host-browser, host-runtime, plugin-api)
- Tool manifest (txt2img, img2img, txt2vid, img2vid, digital_human_talk)
- txt2img full automation flow
- Auth detection, model selection, generation wait helpers
- Rate limiting (2s between actions)"
```

---

## Task 4: Implement WASM plugin runtime in rsclaw

**Files:**
- Create: `src/plugin/wasm_runtime.rs`
- Modify: `src/plugin/mod.rs`

- [ ] **Step 1: Create src/plugin/wasm_runtime.rs — wasmtime loader + host functions**

```rust
//! WASM plugin runtime using wasmtime.
//!
//! Loads `.wasm` files from the plugins directory, provides host functions
//! (browser automation, logging, file access), and dispatches tool calls.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{info, warn, debug};
use wasmtime::*;

use crate::browser::BrowserSession;

/// A loaded WASM plugin instance.
pub struct WasmPlugin {
    /// Plugin name (from manifest).
    pub name: String,
    /// Tool definitions (from get_manifest).
    pub tools: Vec<WasmToolDef>,
    /// Path to .wasm file.
    path: PathBuf,
    /// Shared browser session.
    browser: Arc<Mutex<Option<BrowserSession>>>,
    /// Wasmtime engine (shared across plugins).
    engine: Engine,
    /// Compiled module.
    module: Module,
}

#[derive(Debug, Clone)]
pub struct WasmToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl WasmPlugin {
    /// Load a WASM plugin from a .wasm file.
    pub fn load(
        path: &Path,
        browser: Arc<Mutex<Option<BrowserSession>>>,
        engine: &Engine,
    ) -> Result<Self> {
        let module = Module::from_file(engine, path)?;

        let mut plugin = Self {
            name: String::new(),
            tools: Vec::new(),
            path: path.to_path_buf(),
            browser,
            engine: engine.clone(),
            module,
        };

        // Call get_manifest to populate name and tools.
        let manifest_json = plugin.call_sync("get_manifest", "")?;
        let manifest: Value = serde_json::from_str(&manifest_json)?;

        plugin.name = manifest.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        if let Some(tools) = manifest.get("tools").and_then(|v| v.as_array()) {
            for t in tools {
                plugin.tools.push(WasmToolDef {
                    name: t.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    description: t.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    parameters: t.get("parameters").cloned().unwrap_or(json!({})),
                });
            }
        }

        info!(plugin = %plugin.name, tools = plugin.tools.len(), "WASM plugin loaded");
        Ok(plugin)
    }

    /// Call a plugin export function synchronously (for get_manifest).
    fn call_sync(&self, func_name: &str, args: &str) -> Result<String> {
        // Simplified sync call — used only for get_manifest at load time.
        // For tool calls, use call_tool which runs in async context.
        let mut store = Store::new(&self.engine, HostState::default());
        let linker = create_linker(&self.engine)?;
        let instance = linker.instantiate(&mut store, &self.module)?;

        let func = instance.get_typed_func::<(&str, &str), (i32, &str)>(&mut store, func_name)
            .or_else(|_| {
                // Try as export with no args for get_manifest
                let f = instance.get_typed_func::<(), &str>(&mut store, func_name)?;
                Ok::<_, anyhow::Error>(todo!("handle no-arg export"))
            });

        // For now, use a simpler approach: call via memory interface
        todo!("implement WASM function calls via component model")
    }

    /// Handle a tool call from the agent.
    pub async fn call_tool(&self, tool_name: &str, args: Value) -> Result<Value> {
        let args_json = serde_json::to_string(&args)?;

        // Rate limit: global per-plugin limit
        // TODO: implement rate limiting

        // Instantiate module with host functions bound to our browser session
        let result = self.execute_in_wasm("handle_tool", tool_name, &args_json).await?;

        let parsed: Value = serde_json::from_str(&result)
            .unwrap_or_else(|_| json!({"result": result}));

        Ok(parsed)
    }

    /// Execute a function inside the WASM module with host functions available.
    async fn execute_in_wasm(&self, _export: &str, tool_name: &str, args_json: &str) -> Result<String> {
        // TODO: Full implementation with wasmtime component model.
        // This requires:
        // 1. Create Store with HostState containing browser ref
        // 2. Link host functions (browser_open, browser_snapshot, etc.)
        // 3. Instantiate module
        // 4. Call handle_tool export
        // 5. Return result string

        // Placeholder for now — will be filled in when WIT bindings are generated
        bail!("WASM execution not yet implemented for tool={tool_name} args={args_json}")
    }
}

/// Host state passed to WASM host functions.
#[derive(Default)]
struct HostState {
    browser: Option<Arc<Mutex<Option<BrowserSession>>>>,
}

/// Create a Linker with all host functions registered.
fn create_linker(engine: &Engine) -> Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);

    // TODO: Register host functions (browser_open, browser_snapshot, etc.)
    // These will be implemented as wasmtime host functions that bridge
    // to BrowserSession methods.

    Ok(linker)
}

/// Scan a directory for .wasm plugin files.
pub fn scan_wasm_plugins(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "wasm") {
                found.push(path);
            }
        }
    }
    found
}
```

Note: The full wasmtime component model integration (WIT bindings, host function registration) is complex. Task 4 creates the structure; Task 7 will complete the integration once the plugin compiles to WASM and we can test end-to-end.

- [ ] **Step 2: Update src/plugin/mod.rs — add wasm_runtime module**

Add after line 3 (`pub mod slots;`):

```rust
pub mod wasm_runtime;
```

And add to the re-exports:

```rust
pub use wasm_runtime::{WasmPlugin, WasmToolDef, scan_wasm_plugins};
```

- [ ] **Step 3: Commit**

```bash
cd ~/dev/rsclaw-jimeng
git add src/plugin/wasm_runtime.rs src/plugin/mod.rs
git commit -m "feat(plugin): add WASM plugin runtime scaffold with wasmtime"
```

---

## Task 5: Register WASM plugin tools in agent tool list

**Files:**
- Modify: `src/agent/tools_builder.rs`
- Modify: `src/agent/runtime.rs`

- [ ] **Step 1: Add WASM plugin tools to the tool list builder**

In `src/agent/tools_builder.rs`, find the end of the `build_tools` function (after the last `tools.push(...)`) and add:

```rust
    // WASM plugin tools
    if let Some(ref plugins) = wasm_plugins {
        for plugin in plugins {
            for tool in &plugin.tools {
                let full_name = format!("{}.{}", plugin.name, tool.name);
                tools.push(ToolDef {
                    name: full_name,
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                });
            }
        }
    }
```

Update the function signature to accept `wasm_plugins: Option<&[WasmPlugin]>`.

- [ ] **Step 2: Add WASM plugin tool dispatch in runtime.rs**

In the tool dispatch chain (around line 3995), add before the `// 4. Skill tool` section:

```rust
        // 3.5 WASM plugin tool: prefixed with `<plugin_name>.`
        if let Some(ref wasm_plugins) = self.wasm_plugins {
            if let Some((plugin_name, tool_name)) = name.split_once('.') {
                if let Some(plugin) = wasm_plugins.iter().find(|p| p.name == plugin_name) {
                    let result = plugin.call_tool(tool_name, args).await?;
                    return Ok(result);
                }
            }
        }
```

Add `wasm_plugins: Option<Vec<WasmPlugin>>` field to `AgentRuntime`.

- [ ] **Step 3: Commit**

```bash
git add src/agent/tools_builder.rs src/agent/runtime.rs
git commit -m "feat(agent): register and dispatch WASM plugin tools"
```

---

## Task 6: Build jimeng.wasm and test loading

**Files:**
- Modify: `~/dev/rsclaw-plugins/jimeng/` (fix compilation)
- Test: end-to-end load test

- [ ] **Step 1: Install wasm32-wasip2 target**

```bash
rustup target add wasm32-wasip2
```

- [ ] **Step 2: Build the plugin**

```bash
cd ~/dev/rsclaw-plugins/jimeng
cargo build --target wasm32-wasip2 --release
ls -lh target/wasm32-wasip2/release/rsclaw_plugin_jimeng.wasm
```

Expected: `.wasm` file of 1-5MB

- [ ] **Step 3: Install to rsclaw plugins dir**

```bash
mkdir -p ~/.rsclaw-jimeng/plugins
cp target/wasm32-wasip2/release/rsclaw_plugin_jimeng.wasm ~/.rsclaw-jimeng/plugins/jimeng.wasm
```

- [ ] **Step 4: Test loading via rsclaw gateway**

```bash
cd ~/dev/rsclaw-jimeng
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo run -- --profile jimeng gateway start
```

Check logs for: `WASM plugin loaded: jimeng (N tools)`

- [ ] **Step 5: Test tool call via curl**

```bash
curl -s http://127.0.0.1:19016/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"用即梦生成图片：小猫在草地上"}],"stream":false}'
```

Expected: agent calls `jimeng.txt2img` tool

- [ ] **Step 6: Commit**

```bash
cd ~/dev/rsclaw-plugins
git add -A
git commit -m "feat(jimeng): first compilable WASM build"
```

---

## Task 7: Complete wasmtime host function integration

This task completes the TODO stubs in `wasm_runtime.rs` with full wasmtime component model integration. Depends on Task 6 producing a valid `.wasm` file to test against.

**Files:**
- Modify: `src/plugin/wasm_runtime.rs`

- [ ] **Step 1: Implement host function registration with wasmtime component model**

This step requires iterating on the exact wasmtime API based on the WIT bindings generated in Task 6. The implementation bridges each WIT `import` function to the corresponding `BrowserSession` method.

- [ ] **Step 2: Test txt2img end-to-end**

```bash
# Start gateway
cd ~/dev/rsclaw-jimeng
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo run -- --profile jimeng gateway start

# Send message via feishu or curl
curl -s http://127.0.0.1:19016/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"生成图片：一只小猫在草地上奔跑"}],"stream":false}'
```

Expected: Jimeng opens in browser, generates image, downloads, returns path to user.

- [ ] **Step 3: Commit**

```bash
git add src/plugin/wasm_runtime.rs
git commit -m "feat(plugin): complete wasmtime host function integration"
```

---

## Future Tasks (Phase 3-6, separate plans)

- Task 8-10: Implement img2img, txt2vid, img2vid in jimeng plugin
- Task 11-13: Implement digital_human_create/talk/clone
- Task 14+: Canvas tools, story creation, advanced control, utilities
- Task N: Error recovery, progress streaming, result caching
