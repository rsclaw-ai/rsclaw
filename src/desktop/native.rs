//! Native desktop session backed by `enigo` + `xcap`.
//!
//! Input synthesis runs inside `tokio::task::spawn_blocking` with a fresh
//! `Enigo` per call because `Enigo` is not `Send` on Windows. Screenshots
//! are similarly blocking.

use std::process::Command;

use base64::Engine;
use enigo::{
    Axis, Button, Coordinate,
    Direction::{Click, Press, Release},
    Enigo, Key, Keyboard, Mouse, Settings,
};
use image::{ImageFormat, RgbaImage};
use tracing::warn;
use xcap::{Monitor, Window};

use super::DesktopSession;

/// Unit struct — holds no state because every enigo call needs a fresh
/// instance on the calling OS thread.
pub struct NativeDesktopSession;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Construct a fresh `Enigo` or return an error string.
fn new_enigo() -> Result<Enigo, String> {
    Enigo::new(&Settings::default()).map_err(|e| {
        let hint = if cfg!(target_os = "macos")
            && (e.to_string().contains("permission") || e.to_string().contains("simulate input"))
        {
            " (macOS: grant Accessibility + Input Monitoring in System Settings)"
        } else {
            ""
        };
        format!("enigo init failed: {e}{hint}")
    })
}

/// Pass coordinates through to enigo (enigo on macOS uses device-independent
/// points, the same unit returned by AppleScript `position`/`size`).
fn scale_for_input(x: u32, y: u32) -> (i32, i32) {
    (x as i32, y as i32)
}

/// Capture a region of the primary monitor.
fn capture_region(x: u32, y: u32, w: u32, h: u32) -> Result<String, String> {
    let monitors = Monitor::all().map_err(|e| format!("xcap Monitor::all failed: {e}"))?;
    if monitors.is_empty() {
        return Err("no monitors detected".to_string());
    }
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .unwrap_or(&monitors[0])
        .clone();

    let img: RgbaImage = monitor
        .capture_image()
        .map_err(|e| format!("xcap capture_image failed: {e}"))?;

    let x = x.min(img.width());
    let y = y.min(img.height());
    let w = w.min(img.width() - x);
    let h = h.min(img.height() - y);

    let cropped = image::imageops::crop_imm(&img, x, y, w, h).to_image();
    let mut png_bytes = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut png_bytes);
        cropped
            .write_to(&mut cursor, ImageFormat::Png)
            .map_err(|e| format!("PNG encode failed: {e}"))?;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    Ok(format!("data:image/png;base64,{b64}"))
}

/// Capture the full primary monitor (fallback when window bounds unavailable).
fn capture_primary_monitor() -> Result<String, String> {
    let monitors = Monitor::all().map_err(|e| format!("xcap Monitor::all failed: {e}"))?;
    if monitors.is_empty() {
        return Err("no monitors detected".to_string());
    }
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .unwrap_or(&monitors[0])
        .clone();

    let img: RgbaImage = monitor
        .capture_image()
        .map_err(|e| format!("xcap capture_image failed: {e}"))?;

    let mut png_bytes = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut png_bytes);
        img.write_to(&mut cursor, ImageFormat::Png)
            .map_err(|e| format!("PNG encode failed: {e}"))?;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    Ok(format!("data:image/png;base64,{b64}"))
}

/// Capture an app window's content directly by window backing store
/// (`CGWindowListCreateImage` under xcap). Overlap-proof — works even when
/// other windows cover it and without bringing the app frontmost — and does
/// NOT trigger any app-side screenshot UI (so it never pollutes a chat input
/// box the way WeChat's Ctrl+Cmd+A capture does).
///
/// `Window::all()` is ordered front-to-back, so the first matching window is
/// the frontmost. Capturing the frontmost window lets one call serve both the
/// main window and a transient child window (e.g. WeChat's merged-record
/// viewer): when the child is open it is frontmost and gets captured;
/// otherwise the main window does. A 200x200 minimum filters out tooltips and
/// other tiny helper windows. Returns a PNG data URI.
fn capture_app_window(app_name: &str) -> Result<String, String> {
    let windows = Window::all().map_err(|e| format!("xcap Window::all failed: {e}"))?;
    let win = windows
        .iter()
        .find(|w| {
            w.app_name().map(|n| n == app_name).unwrap_or(false)
                && !w.is_minimized().unwrap_or(true)
                && w.width().unwrap_or(0) >= 200
                && w.height().unwrap_or(0) >= 200
        })
        .ok_or_else(|| format!("no on-screen window for app '{app_name}'"))?;
    let img: RgbaImage = win
        .capture_image()
        .map_err(|e| format!("xcap window capture_image failed: {e}"))?;
    let mut png_bytes = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut png_bytes);
        img.write_to(&mut cursor, ImageFormat::Png)
            .map_err(|e| format!("PNG encode failed: {e}"))?;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    Ok(format!("data:image/png;base64,{b64}"))
}

/// Convert a macOS bundle-id to the process name used by System Events.
/// E.g. "com.tencent.xinWeChat" -> "WeChat", "com.apple.Safari" -> "Safari".
fn bundle_to_app_name(bundle_id: &str) -> String {
    // Common overrides for apps whose process name differs from bundle-id tail.
    match bundle_id {
        "com.tencent.xinWeChat" => "WeChat".to_string(),
        "com.tencent.xinWeChat.desktop" => "WeChat".to_string(),
        _ => {
            // Take the last path component and strip common suffixes.
            let tail = bundle_id.rsplit('.').next().unwrap_or(bundle_id);
            tail.to_string()
        }
    }
}

/// Run an AppleScript via osascript and return stdout as String.
fn run_osascript(script: &str) -> Result<String, String> {
    match Command::new("osascript").arg("-e").arg(script).output() {
        Ok(out) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(out) => Err(format!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )),
        Err(e) => Err(format!("osascript spawn failed: {e}")),
    }
}

/// Convert a key name to an AppleScript key code (macOS).
fn key_to_applescript_code(name: &str) -> Result<u16, String> {
    let code = match name.trim().to_lowercase().as_str() {
        // Letters
        "a" => 0,
        "b" => 11,
        "c" => 8,
        "d" => 2,
        "e" => 14,
        "f" => 3,
        "g" => 5,
        "h" => 4,
        "i" => 34,
        "j" => 38,
        "k" => 40,
        "l" => 37,
        "m" => 46,
        "n" => 45,
        "o" => 31,
        "p" => 35,
        "q" => 12,
        "r" => 15,
        "s" => 1,
        "t" => 17,
        "u" => 32,
        "v" => 9,
        "w" => 13,
        "x" => 7,
        "y" => 16,
        "z" => 6,
        // Numbers
        "0" => 29,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "5" => 23,
        "6" => 22,
        "7" => 26,
        "8" => 28,
        "9" => 25,
        // Special keys
        "return" | "enter" => 36,
        "delete" | "del" => 51,
        "escape" | "esc" => 53,
        "space" | "spacebar" => 49,
        "tab" => 48,
        "up" | "arrowup" | "uparrow" => 126,
        "down" | "arrowdown" | "downarrow" => 125,
        "left" | "arrowleft" | "leftarrow" => 123,
        "right" | "arrowright" | "rightarrow" => 124,
        "pageup" | "pgup" => 116,
        "pagedown" | "pgdn" => 121,
        "home" => 115,
        "end" => 119,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        _ => return Err(format!("unknown key for AppleScript: {name}")),
    };
    Ok(code)
}

/// Convert modifier names to AppleScript modifier string.
fn modifiers_to_applescript(modifiers: &[String]) -> String {
    let mut parts = Vec::new();
    for m in modifiers {
        match m.trim().to_lowercase().as_str() {
            "command" | "cmd" | "meta" | "super" => parts.push("command down"),
            "control" | "ctrl" => parts.push("control down"),
            "shift" => parts.push("shift down"),
            "option" | "alt" => parts.push("option down"),
            _ => {}
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("using {{{}}}", parts.join(", "))
    }
}

/// Parse a key name to enigo::Key.
fn parse_key(name: &str) -> Option<Key> {
    let k = match name.trim().to_lowercase().as_str() {
        "return" | "enter" => Key::Return,
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" | "option" => Key::Alt,
        "cmd" | "command" | "meta" | "win" | "super" => Key::Meta,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "space" | "spacebar" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" | "arrowup" | "uparrow" => Key::UpArrow,
        "down" | "arrowdown" | "downarrow" => Key::DownArrow,
        "left" | "arrowleft" | "leftarrow" => Key::LeftArrow,
        "right" | "arrowright" | "rightarrow" => Key::RightArrow,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "home" => Key::Home,
        "end" => Key::End,
        "capslock" => Key::CapsLock,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        s if s.chars().count() == 1 => Key::Unicode(s.chars().next()?),
        _ => return None,
    };
    Some(k)
}

/// Fallback to Core Graphics window list when AppleScript/System Events
/// can't enumerate windows (common with sandboxed apps like WeChat).
/// Only works on macOS; returns an error on other platforms.
fn cgwindow_fallback(owner_name: &str) -> Result<String, String> {
    let owner_escaped = owner_name.replace('"', r#"\""#).replace('\'', "'\"'\"'");
    let py = format!(
        r#"import Quartz, json
wl = Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionAll, Quartz.kCGNullWindowID)
best = None
best_area = 0
for w in wl:
    if w.get('kCGWindowOwnerName','') != '{}':
        continue
    b = w.get('kCGWindowBounds',{{}})
    x, y, wi, h = int(b.get('X',0)), int(b.get('Y',0)), int(b.get('Width',0)), int(b.get('Height',0))
    if wi < 200 or h < 200:
        continue
    area = wi * h
    if area > best_area:
        best_area = area
        best = (x, y, wi, h)
if best:
    print(json.dumps({{'x':best[0],'y':best[1],'w':best[2],'h':best[3]}}))
else:
    print('')
"#,
        owner_escaped
    );
    match Command::new("python3").args(["-c", &py]).output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if text.is_empty() {
                Err(format!(
                    "cgwindow_fallback: no {owner_name} window found via CGWindowList"
                ))
            } else {
                Ok(text)
            }
        }
        Ok(out) => Err(format!(
            "cgwindow_fallback: python3 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )),
        Err(e) => Err(format!("cgwindow_fallback: python3 spawn failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// DesktopSession impl
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl DesktopSession for NativeDesktopSession {
    async fn activate_app(&self, bundle_id: &str) -> Result<String, String> {
        let bundle_id = bundle_id.to_owned();
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                // Three-layer fallback (matches wechat_agent.py):
                // 1. open -b (system-level launchctl, most reliable)
                // 2. activate via AppleScript
                // 3. set frontmost via System Events
                // 4. Poll frontmost up to 5 times
                let _ = Command::new("open")
                    .args(["-b", &bundle_id])
                    .output();
                std::thread::sleep(std::time::Duration::from_millis(500));

                let script_activate = format!(
                    r#"tell application id "{}" to activate"#,
                    bundle_id.replace('"', r#"\""#)
                );
                let _ = Command::new("osascript")
                    .args(["-e", &script_activate])
                    .output();
                std::thread::sleep(std::time::Duration::from_millis(300));

                let script_frontmost = format!(
                    r#"tell application "System Events"
    set frontmost of (first process whose bundle identifier is "{}") to true
end tell"#,
                    bundle_id.replace('"', r#"\""#)
                );
                let _ = Command::new("osascript")
                    .args(["-e", &script_frontmost])
                    .output();

                // Verify frontmost up to 5 times
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    let check = format!(
                        r#"tell application "System Events"
    return frontmost of (first process whose bundle identifier is "{}")
end tell"#,
                        bundle_id.replace('"', r#"\""#)
                    );
                    match Command::new("osascript").args(["-e", &check]).output() {
                        Ok(out) if out.status.success() => {
                            let result = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
                            if result == "true" {
                                return Ok("ok".to_string());
                            }
                        }
                        _ => {}
                    }
                    let _ = Command::new("open")
                        .args(["-b", &bundle_id])
                        .output();
                }
                Ok("ok".to_string()) // Return ok even if frontmost check fails
            } else if cfg!(target_os = "windows") {
                let escaped = bundle_id
                    .replace('`', "``")
                    .replace('*', "`*")
                    .replace('?', "`?")
                    .replace('[', "`[")
                    .replace(']', "`]")
                    .replace('\'', "''");
                let ps = format!(
                    r#"Add-Type -Name W -Namespace N -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);'; Get-Process | Where-Object {{$_.ProcessName -like '*{}*'}} | ForEach-Object {{ if ($_.MainWindowHandle -ne 0) {{ [N.W]::SetForegroundWindow($_.MainWindowHandle) }} }}"#,
                    escaped
                );
                #[allow(unused_mut)]
                let mut ps_cmd = Command::new("powershell");
                ps_cmd.args(["-NoProfile", "-Command", &ps]);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    ps_cmd.creation_flags(0x08000000);
                }
                match ps_cmd.output() {
                    Ok(out) if out.status.success() => Ok("ok".to_string()),
                    Ok(out) => Err(format!("powershell failed: {}", String::from_utf8_lossy(&out.stderr))),
                    Err(e) => Err(format!("powershell spawn failed: {e}")),
                }
            } else if cfg!(target_os = "linux") {
                let wmctrl = Command::new("wmctrl").args(["-a", &bundle_id]).status();
                if matches!(&wmctrl, Ok(s) if s.success()) {
                    return Ok("ok".to_string());
                }
                match Command::new("xdotool")
                    .args(["search", "--class", &bundle_id, "windowactivate"])
                    .status()
                {
                    Ok(s) if s.success() => Ok("ok".to_string()),
                    Ok(s) => Err(format!("xdotool exit status: {s}")),
                    Err(e) => Err(format!("neither wmctrl nor xdotool worked: {e}")),
                }
            } else {
                Err("activate_app: unsupported platform".to_string())
            }
        })
        .await
        .map_err(|e| format!("activate_app join failed: {e}"))?
    }

    async fn list_windows(&self, bundle_id: &str) -> Result<String, String> {
        let bundle_id = bundle_id.to_owned();
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                let app_name = bundle_to_app_name(&bundle_id);
                let script = format!(
                    r#"tell application "System Events" to tell process "{}" to set winList to {{}}
repeat with i from 1 to (count windows)
    set w to window i
    set wName to name of w
    set wPos to position of w
    set wSize to size of w
    set wInfo to "{{\"idx\":" & i & ",\"title\":\"" & wName & "\",\"x\":" & (item 1 of wPos) & ",\"y\":" & (item 2 of wPos) & ",\"w\":" & (item 1 of wSize) & ",\"h\":" & (item 2 of wSize) & "}}"
    set end of winList to wInfo
end repeat
set AppleScript's text item delimiters to ","
return "[" & (winList as string) & "]"
"#,
                    app_name.replace('"', r#"\""#)
                );
                match run_osascript(&script) {
                    Ok(json) => Ok(json),
                    Err(e) => Err(format!("list_windows failed: {e}")),
                }
            } else {
                Err("list_windows: not yet implemented on this platform".to_string())
            }
        })
        .await
        .map_err(|e| format!("list_windows join failed: {e}"))?
    }

    async fn close_window(&self, bundle_id: &str, window_idx: u32) -> Result<String, String> {
        let bundle_id = bundle_id.to_owned();
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                let app_name = bundle_to_app_name(&bundle_id);
                let script = format!(
                    r#"tell application "System Events" to tell process "{}" to click button 1 of window {}"#,
                    app_name.replace('"', r#"\""#),
                    window_idx
                );
                match run_osascript(&script) {
                    Ok(_) => Ok("ok".to_string()),
                    Err(e) => Err(format!("close_window failed: {e}")),
                }
            } else {
                Err("close_window: not yet implemented on this platform".to_string())
            }
        })
        .await
        .map_err(|e| format!("close_window join failed: {e}"))?
    }

    async fn get_main_window(&self, bundle_id: &str) -> Result<String, String> {
        let bundle_id = bundle_id.to_owned();
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                let app_name = bundle_to_app_name(&bundle_id);
                // Try AppleScript first (most accurate when Accessibility works).
                let script = format!(
                    r#"tell application "System Events" to tell process "{}"
    set winCount to count of windows
    if winCount = 0 then return "0,0,0,0"
    set targetWin to missing value
    repeat with w in windows
        if name of w contains "Weixin" or name of w contains "WeChat" then
            set targetWin to w
            exit repeat
        end if
    end repeat
    if targetWin is missing value then set targetWin to window 1
    set p to position of targetWin
    set s to size of targetWin
    return (item 1 of p as text) & "," & (item 2 of p as text) & "," & (item 1 of s as text) & "," & (item 2 of s as text)
end tell"#,
                    app_name.replace('"', r#"\""#)
                );
                match run_osascript(&script) {
                    Ok(output) => {
                        let trimmed = output.trim();
                        if trimmed == "0,0,0,0" {
                            // AppleScript sees 0 windows — fall back to Core Graphics.
                            return cgwindow_fallback("WeChat");
                        }
                        let parts: Vec<&str> = trimmed.split(',').collect();
                        if parts.len() == 4 {
                            Ok(format!(
                                "{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
                                parts[0], parts[1], parts[2], parts[3]
                            ))
                        } else {
                            Err(format!("get_main_window: unexpected output: {output}"))
                        }
                    }
                    Err(_) => cgwindow_fallback("WeChat"),
                }
            } else {
                Err("get_main_window: not yet implemented on this platform".to_string())
            }
        })
        .await
        .map_err(|e| format!("get_main_window join failed: {e}"))?
    }

    async fn screenshot_window(&self, bundle_id: &str) -> Result<String, String> {
        // Primary path: direct window-backing-store capture (overlap-proof,
        // no app-side screenshot UI, so it never pollutes a chat input box).
        let app_name = bundle_to_app_name(bundle_id);
        match tokio::task::spawn_blocking(move || capture_app_window(&app_name))
            .await
            .map_err(|e| format!("screenshot_window join failed: {e}"))?
        {
            Ok(image) => return Ok(image),
            Err(e) => warn!("capture_app_window failed ({e}); falling back to region capture"),
        }
        // Fallback: main window bounds + region crop, then primary monitor.
        // (region crop captures whatever is on top of the rect, so it is less
        // reliable when other windows overlap — used only if direct fails.)
        match self.get_main_window(bundle_id).await {
            Ok(bounds) => {
                let parsed: serde_json::Value = serde_json::from_str(&bounds)
                    .map_err(|e| format!("screenshot_window: failed to parse bounds: {e}"))?;
                let x = parsed["x"].as_u64().unwrap_or(0) as u32;
                let y = parsed["y"].as_u64().unwrap_or(0) as u32;
                let w = parsed["w"].as_u64().unwrap_or(0) as u32;
                let h = parsed["h"].as_u64().unwrap_or(0) as u32;
                self.screenshot_region(x, y, w, h).await
            }
            Err(_) => {
                // Fallback: capture primary monitor.
                tokio::task::spawn_blocking(move || capture_primary_monitor())
                    .await
                    .map_err(|e| format!("screenshot_window fallback join failed: {e}"))?
            }
        }
    }

    async fn screenshot_region(&self, x: u32, y: u32, w: u32, h: u32) -> Result<String, String> {
        tokio::task::spawn_blocking(move || capture_region(x, y, w, h))
            .await
            .map_err(|e| format!("screenshot_region join failed: {e}"))?
    }

    async fn mouse_move(&self, x: u32, y: u32) -> Result<String, String> {
        tokio::task::spawn_blocking(move || {
            let mut enigo = new_enigo()?;
            let (lx, ly) = scale_for_input(x, y);
            enigo
                .move_mouse(lx, ly, Coordinate::Abs)
                .map_err(|e| format!("move_mouse: {e}"))?;
            Ok("ok".to_string())
        })
        .await
        .map_err(|e| format!("mouse_move join failed: {e}"))?
    }

    async fn mouse_click(&self, x: u32, y: u32) -> Result<String, String> {
        tokio::task::spawn_blocking(move || {
            let mut enigo = new_enigo()?;
            let (lx, ly) = scale_for_input(x, y);
            enigo
                .move_mouse(lx, ly, Coordinate::Abs)
                .map_err(|e| format!("move_mouse: {e}"))?;
            enigo
                .button(Button::Left, Click)
                .map_err(|e| format!("button click: {e}"))?;
            Ok("ok".to_string())
        })
        .await
        .map_err(|e| format!("mouse_click join failed: {e}"))?
    }

    async fn mouse_double_click(&self, x: u32, y: u32) -> Result<String, String> {
        tokio::task::spawn_blocking(move || {
            let mut enigo = new_enigo()?;
            let (lx, ly) = scale_for_input(x, y);
            enigo
                .move_mouse(lx, ly, Coordinate::Abs)
                .map_err(|e| format!("move_mouse: {e}"))?;
            enigo
                .button(Button::Left, Click)
                .map_err(|e| format!("button click 1: {e}"))?;
            std::thread::sleep(std::time::Duration::from_millis(80));
            enigo
                .button(Button::Left, Click)
                .map_err(|e| format!("button click 2: {e}"))?;
            Ok("ok".to_string())
        })
        .await
        .map_err(|e| format!("mouse_double_click join failed: {e}"))?
    }

    async fn mouse_drag(&self, x1: u32, y1: u32, x2: u32, y2: u32) -> Result<String, String> {
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                // macOS: use Python+Quartz for reliable multi-step dragging.
                // enigo's move_mouse does not produce the exact CGEvent sequence
                // that WeChat's screenshot overlay requires.
                let py = format!(
                    r#"import time, Quartz
steps = 20
x1, y1, x2, y2 = {}, {}, {}, {}

# mouseDown @ start
Quartz.CGEventPost(Quartz.kCGHIDEventTap,
    Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventLeftMouseDown,
        Quartz.CGPoint(x1, y1), Quartz.kCGMouseButtonLeft))
time.sleep(0.1)

# mouseDragged 20 times
for i in range(1, steps + 1):
    t = i / steps
    cx = x1 + (x2 - x1) * t
    cy = y1 + (y2 - y1) * t
    Quartz.CGEventPost(Quartz.kCGHIDEventTap,
        Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventLeftMouseDragged,
            Quartz.CGPoint(cx, cy), Quartz.kCGMouseButtonLeft))
    time.sleep(0.025)

# mouseUp @ end
time.sleep(0.1)
Quartz.CGEventPost(Quartz.kCGHIDEventTap,
    Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventLeftMouseUp,
        Quartz.CGPoint(x2, y2), Quartz.kCGMouseButtonLeft))
print('ok')
"#,
                    x1, y1, x2, y2
                );
                match Command::new("python3").args(["-c", &py]).output() {
                    Ok(out) if out.status.success() => Ok("ok".to_string()),
                    Ok(out) => Err(format!(
                        "mouse_drag: python3 failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )),
                    Err(e) => Err(format!("mouse_drag: python3 spawn failed: {e}")),
                }
            } else {
                let mut enigo = new_enigo()?;
                let (fx, fy) = scale_for_input(x1, y1);
                let (tx, ty) = scale_for_input(x2, y2);
                enigo
                    .move_mouse(fx, fy, Coordinate::Abs)
                    .map_err(|e| format!("move_mouse start: {e}"))?;
                enigo
                    .button(Button::Left, Press)
                    .map_err(|e| format!("button press: {e}"))?;
                enigo
                    .move_mouse(tx, ty, Coordinate::Abs)
                    .map_err(|e| format!("move_mouse end: {e}"))?;
                enigo
                    .button(Button::Left, Release)
                    .map_err(|e| format!("button release: {e}"))?;
                Ok("ok".to_string())
            }
        })
        .await
        .map_err(|e| format!("mouse_drag join failed: {e}"))?
    }

    async fn mouse_scroll(&self, clicks: i32) -> Result<String, String> {
        tokio::task::spawn_blocking(move || {
            let mut enigo = new_enigo()?;
            let axis = Axis::Vertical;
            enigo
                .scroll(clicks, axis)
                .map_err(|e| format!("scroll: {e}"))?;
            Ok("ok".to_string())
        })
        .await
        .map_err(|e| format!("mouse_scroll join failed: {e}"))?
    }

    async fn key_press(&self, key: &str, modifiers: &[String]) -> Result<String, String> {
        let key = key.to_owned();
        let modifiers: Vec<String> = modifiers.to_vec();
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                // macOS: use AppleScript/System Events for reliable shortcut delivery
                // (enigo sends global CGEvents which some sandboxed apps ignore).
                let target_code = key_to_applescript_code(&key)?;
                let mod_script = modifiers_to_applescript(&modifiers);
                let script = format!(
                    r#"tell application "System Events" to key code {} {}"#,
                    target_code, mod_script
                );
                run_osascript(&script)
            } else {
                let mut enigo = new_enigo()?;
                let target = parse_key(&key).ok_or_else(|| format!("unknown key: {key}"))?;
                let mut mod_keys = Vec::new();
                for m in &modifiers {
                    let mk = parse_key(m).ok_or_else(|| format!("unknown modifier: {m}"))?;
                    mod_keys.push(mk);
                }
                for mk in &mod_keys {
                    enigo
                        .key(*mk, Press)
                        .map_err(|e| format!("modifier press: {e}"))?;
                }
                enigo
                    .key(target, Click)
                    .map_err(|e| format!("key press: {e}"))?;
                for mk in mod_keys.iter().rev() {
                    if let Err(e) = enigo.key(*mk, Release) {
                        warn!(error = %e, "modifier release error (best-effort)");
                    }
                }
                Ok("ok".to_string())
            }
        })
        .await
        .map_err(|e| format!("key_press join failed: {e}"))?
    }

    async fn clipboard_set(&self, text: &str) -> Result<String, String> {
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                match Command::new("pbcopy")
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(mut child) => {
                        use std::io::Write;
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(text.as_bytes());
                        }
                        match child.wait() {
                            Ok(s) if s.success() => Ok("ok".to_string()),
                            Ok(s) => Err(format!("pbcopy exit status: {s}")),
                            Err(e) => Err(format!("pbcopy wait failed: {e}")),
                        }
                    }
                    Err(e) => Err(format!("pbcopy spawn failed: {e}")),
                }
            } else if cfg!(target_os = "linux") {
                let mut cmd = Command::new("xclip")
                    .args(["-selection", "clipboard"])
                    .stdin(std::process::Stdio::piped())
                    .spawn();
                if cmd.is_err() {
                    cmd = Command::new("xsel")
                        .args(["--clipboard", "--input"])
                        .stdin(std::process::Stdio::piped())
                        .spawn();
                }
                match cmd {
                    Ok(mut child) => {
                        use std::io::Write;
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(text.as_bytes());
                        }
                        match child.wait() {
                            Ok(s) if s.success() => Ok("ok".to_string()),
                            Ok(s) => Err(format!("xclip/xsel exit status: {s}")),
                            Err(e) => Err(format!("xclip/xsel wait failed: {e}")),
                        }
                    }
                    Err(e) => Err(format!("no clipboard tool available: {e}")),
                }
            } else if cfg!(target_os = "windows") {
                let ps = format!("Set-Clipboard -Value '{}'", text.replace('\'', "''"));
                #[allow(unused_mut)]
                let mut ps_cmd = Command::new("powershell");
                ps_cmd.args(["-NoProfile", "-Command", &ps]);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    ps_cmd.creation_flags(0x08000000);
                }
                match ps_cmd.output()
                {
                    Ok(out) if out.status.success() => Ok("ok".to_string()),
                    Ok(out) => Err(format!(
                        "Set-Clipboard failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )),
                    Err(e) => Err(format!("powershell spawn failed: {e}")),
                }
            } else {
                Err("clipboard_set: unsupported platform".to_string())
            }
        })
        .await
        .map_err(|e| format!("clipboard_set join failed: {e}"))?
    }

    async fn clipboard_get(&self) -> Result<String, String> {
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                match Command::new("pbpaste").output() {
                    Ok(out) if out.status.success() => {
                        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
                    }
                    Ok(out) => Err(format!(
                        "pbpaste failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )),
                    Err(e) => Err(format!("pbpaste spawn failed: {e}")),
                }
            } else if cfg!(target_os = "linux") {
                let out = Command::new("xclip")
                    .args(["-selection", "clipboard", "-o"])
                    .output();
                let out = if out.is_err() {
                    Command::new("xsel")
                        .args(["--clipboard", "--output"])
                        .output()
                } else {
                    out
                };
                match out {
                    Ok(out) if out.status.success() => {
                        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
                    }
                    Ok(out) => Err(format!(
                        "xclip/xsel failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )),
                    Err(e) => Err(format!("no clipboard tool available: {e}")),
                }
            } else if cfg!(target_os = "windows") {
                #[allow(unused_mut)]
                let mut ps_cmd = Command::new("powershell");
                ps_cmd.args(["-NoProfile", "-Command", "Get-Clipboard"]);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    ps_cmd.creation_flags(0x08000000);
                }
                match ps_cmd.output()
                {
                    Ok(out) if out.status.success() => {
                        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
                    }
                    Ok(out) => Err(format!(
                        "Get-Clipboard failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )),
                    Err(e) => Err(format!("powershell spawn failed: {e}")),
                }
            } else {
                Err("clipboard_get: unsupported platform".to_string())
            }
        })
        .await
        .map_err(|e| format!("clipboard_get join failed: {e}"))?
    }

    async fn clipboard_set_file(&self, file_path: &str) -> Result<String, String> {
        let file_path = file_path.to_owned();
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                let script = format!(
                    "set the clipboard to (POSIX file \"{}\")",
                    file_path.replace('"', "\\\"").replace('\\', "\\\\")
                );
                match Command::new("osascript").args(["-e", &script]).output() {
                    Ok(out) if out.status.success() => Ok("ok".to_string()),
                    Ok(out) => Err(format!(
                        "osascript failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )),
                    Err(e) => Err(format!("osascript spawn failed: {e}")),
                }
            } else if cfg!(target_os = "linux") {
                // xclip with target uri-list for file references
                let uri = format!("file://{file_path}\n");
                match Command::new("xclip")
                    .args(["-selection", "clipboard", "-t", "text/uri-list"])
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(mut child) => {
                        use std::io::Write;
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(uri.as_bytes());
                        }
                        match child.wait() {
                            Ok(s) if s.success() => Ok("ok".to_string()),
                            Ok(s) => Err(format!("xclip exit status: {s}")),
                            Err(e) => Err(format!("xclip wait failed: {e}")),
                        }
                    }
                    Err(e) => Err(format!("xclip spawn failed: {e}")),
                }
            } else if cfg!(target_os = "windows") {
                let ps = format!("Set-Clipboard -Path '{}'", file_path.replace('\'', "''"));
                #[allow(unused_mut)]
                let mut ps_cmd = Command::new("powershell");
                ps_cmd.args(["-NoProfile", "-Command", &ps]);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    ps_cmd.creation_flags(0x08000000);
                }
                match ps_cmd.output()
                {
                    Ok(out) if out.status.success() => Ok("ok".to_string()),
                    Ok(out) => Err(format!(
                        "Set-Clipboard failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )),
                    Err(e) => Err(format!("powershell spawn failed: {e}")),
                }
            } else {
                Err("clipboard_set_file: unsupported platform".to_string())
            }
        })
        .await
        .map_err(|e| format!("clipboard_set_file join failed: {e}"))?
    }
    async fn clipboard_get_image(&self) -> Result<String, String> {
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                let tmp = format!("/tmp/rsclaw_cb_{}.png", std::process::id());
                let py = format!(
                    r#"from AppKit import NSPasteboard, NSBitmapImageRep, NSPasteboardTypeTIFF, NSPNGFileType
import sys
pb = NSPasteboard.generalPasteboard()
data = pb.dataForType_(NSPasteboardTypeTIFF)
if data is None:
    print('CLIPBOARD_EMPTY', file=sys.stderr)
    sys.exit(1)
rep = NSBitmapImageRep.imageRepWithData_(data)
if rep is None:
    print('REP_NONE', file=sys.stderr)
    sys.exit(1)
png = rep.representationUsingType_properties_(NSPNGFileType, None)
if png is None:
    print('PNG_NONE', file=sys.stderr)
    sys.exit(1)
with open('{}', 'wb') as f:
    f.write(bytes(png))
print('ok')
"#,
                    tmp
                );
                match Command::new("python3").args(["-c", &py]).output() {
                    Ok(out) if out.status.success() => {
                        match std::fs::read(&tmp) {
                            Ok(bytes) => {
                                let _ = std::fs::remove_file(&tmp);
                                if bytes.is_empty() {
                                    return Err("clipboard_get_image: empty image".to_string());
                                }
                                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                Ok(format!("data:image/png;base64,{b64}"))
                            }
                            Err(e) => Err(format!("clipboard_get_image: read temp file: {e}")),
                        }
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        if stderr.contains("CLIPBOARD_EMPTY") {
                            return Err("clipboard_get_image: clipboard has no image (screenshot likely failed)".to_string());
                        }
                        Err(format!(
                            "clipboard_get_image: python3 failed: {}",
                            stderr
                        ))
                    }
                    Err(e) => Err(format!("clipboard_get_image: python3 spawn failed: {e}")),
                }
            } else {
                Err("clipboard_get_image: not yet implemented on this platform".to_string())
            }
        })
        .await
        .map_err(|e| format!("clipboard_get_image join failed: {e}"))?
    }

    async fn mouse_right_click(&self, x: u32, y: u32) -> Result<String, String> {
        tokio::task::spawn_blocking(move || {
            let mut enigo = new_enigo()?;
            let (lx, ly) = scale_for_input(x, y);
            enigo
                .move_mouse(lx, ly, Coordinate::Abs)
                .map_err(|e| format!("move_mouse: {e}"))?;
            enigo
                .button(Button::Right, Click)
                .map_err(|e| format!("right button click: {e}"))?;
            Ok("ok".to_string())
        })
        .await
        .map_err(|e| format!("mouse_right_click join failed: {e}"))?
    }

    async fn file_dialog_open(&self, title: &str, _filters: &[String]) -> Result<String, String> {
        let title = title.to_owned();
        tokio::task::spawn_blocking(move || {
            if cfg!(target_os = "macos") {
                let script = format!(
                    r#"choose file with prompt "{}""#,
                    title.replace('"', "\\\"")
                );
                match Command::new("osascript").args(["-e", &script]).output() {
                    Ok(out) if out.status.success() => {
                        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        // AppleScript returns alias like "alias Macintosh HD:Users:..."
                        // Convert to POSIX path
                        if path.starts_with("alias ") {
                            let alias_path = path.strip_prefix("alias ").unwrap_or(&path);
                            match Command::new("osascript")
                                .args([
                                    "-e",
                                    &format!("POSIX path of {} \"{}\"", "alias", alias_path),
                                ])
                                .output()
                            {
                                Ok(out2) if out2.status.success() => {
                                    Ok(String::from_utf8_lossy(&out2.stdout).trim().to_string())
                                }
                                _ => Ok(path),
                            }
                        } else {
                            Ok(path)
                        }
                    }
                    Ok(out) => Err(format!(
                        "osascript failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )),
                    Err(e) => Err(format!("osascript spawn failed: {e}")),
                }
            } else {
                Err("file_dialog_open: only macOS supported".to_string())
            }
        })
        .await
        .map_err(|e| format!("file_dialog_open join failed: {e}"))?
    }
}
