//! Computer-use tool method (screen capture, mouse, keyboard, window management).
//!
//! Split from `runtime.rs` to reduce file size.  All methods live in
//! `impl AgentRuntime` via the split-impl pattern (same struct, different file).

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::platform::{
    jpeg_dimensions, is_cliclick_special_key, map_modifier, map_modifier_xdotool,
    powershell_hidden, run_powershell_input, run_subprocess, win_map_key, win_mouse_click,
    win_set_cursor,
};

impl super::runtime::AgentRuntime {
    pub(crate) async fn tool_computer_use(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("computer_use: `action` required"))?;

        let is_macos = cfg!(target_os = "macos");
        let is_windows = cfg!(target_os = "windows");

        // Helper: extract x, y from args
        let xy = || {
            (
                args["x"].as_f64().unwrap_or(0.0) as i64,
                args["y"].as_f64().unwrap_or(0.0) as i64,
            )
        };

        match action {
            // =================================================================
            // Screenshot — capture + auto-resize for HiDPI (saves tokens)
            // =================================================================
            "screenshot" => {
                let tmp_path = std::env::temp_dir().join("rsclaw_screen.png");
                let tmp_path_str = tmp_path.to_string_lossy().to_string();

                let output = if is_macos {
                    tokio::process::Command::new("screencapture")
                        .args(["-x", &tmp_path_str])
                        .output()
                        .await
                } else if is_windows {
                    let script = format!(
                        r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$screen = [System.Windows.Forms.Screen]::PrimaryScreen
$bounds = $screen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bitmap.Save('{}')
$graphics.Dispose()
$bitmap.Dispose()
"#,
                        tmp_path_str
                    );
                    powershell_hidden()
                        .args(["-Command", &script])
                        .output()
                        .await
                } else {
                    let res = tokio::process::Command::new("scrot")
                        .arg(&tmp_path_str)
                        .output()
                        .await;
                    if res.is_err() || !res.as_ref().unwrap().status.success() {
                        tokio::process::Command::new("import")
                            .args(["-window", "root", &tmp_path_str])
                            .output()
                            .await
                    } else {
                        res
                    }
                }
                .map_err(|e| anyhow!("computer_use screenshot: {e}"))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(anyhow!("computer_use screenshot failed: {stderr}"));
                }

                // Read raw PNG and get original dimensions from header.
                let raw_bytes = tokio::fs::read(&tmp_path)
                    .await
                    .map_err(|e| anyhow!("computer_use: failed to read screenshot: {e}"))?;
                let (orig_w, orig_h) = if raw_bytes.len() >= 24 {
                    let w = u32::from_be_bytes([raw_bytes[16], raw_bytes[17], raw_bytes[18], raw_bytes[19]]);
                    let h = u32::from_be_bytes([raw_bytes[20], raw_bytes[21], raw_bytes[22], raw_bytes[23]]);
                    (w, h)
                } else {
                    (0, 0)
                };

                // Resize to 1024px wide + convert to JPG q30 (~60KB).
                // Matches Anthropic's recommended XGA (1024x768) and saves
                // ~5-10x bandwidth vs raw PNG while maintaining OCR quality.
                const TARGET_WIDTH: u32 = 1024;
                const JPG_QUALITY: u32 = 30;

                let out_path = std::env::temp_dir().join("rsclaw_screen_out.jpg");
                let out_str = out_path.to_string_lossy().to_string();
                let need_resize = orig_w > TARGET_WIDTH;

                let converted = if is_macos {
                    // sips: resize + convert to JPEG in one pass
                    let mut sips_args = vec![];
                    if need_resize {
                        sips_args.extend_from_slice(&["--resampleWidth", "1024"]);
                    }
                    sips_args.extend_from_slice(&[
                        "-s", "format", "jpeg",
                        "-s", "formatOptions", "30",
                        &tmp_path_str,
                        "--out", &out_str,
                    ]);
                    tokio::process::Command::new("sips")
                        .args(&sips_args)
                        .output()
                        .await
                        .map(|o| o.status.success())
                        .unwrap_or(false)
                } else if is_windows {
                    let new_w = if need_resize { TARGET_WIDTH } else { orig_w };
                    let new_h = if need_resize {
                        (orig_h as f64 * TARGET_WIDTH as f64 / orig_w as f64) as u32
                    } else {
                        orig_h
                    };
                    let script = format!(
                        r#"
Add-Type -AssemblyName System.Drawing
$src = [System.Drawing.Image]::FromFile('{tmp_path_str}')
$dst = New-Object System.Drawing.Bitmap({new_w}, {new_h})
$g = [System.Drawing.Graphics]::FromImage($dst)
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.DrawImage($src, 0, 0, {new_w}, {new_h})
$codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object {{ $_.MimeType -eq 'image/jpeg' }}
$params = New-Object System.Drawing.Imaging.EncoderParameters(1)
$params.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter([System.Drawing.Imaging.Encoder]::Quality, [long]{JPG_QUALITY})
$dst.Save('{out_str}', $codec, $params)
$g.Dispose(); $dst.Dispose(); $src.Dispose()
"#
                    );
                    powershell_hidden()
                        .args(["-Command", &script])
                        .output()
                        .await
                        .map(|o| o.status.success())
                        .unwrap_or(false)
                } else {
                    // Linux: convert (ImageMagick)
                    let resize_arg = if need_resize { "1024x" } else { "100%" };
                    tokio::process::Command::new("convert")
                        .args([&tmp_path_str, "-resize", resize_arg, "-quality", "30", &out_str])
                        .output()
                        .await
                        .map(|o| o.status.success())
                        .unwrap_or(false)
                };

                // Use converted JPG if available, otherwise fall back to raw PNG.
                let (bytes, mime) = if converted {
                    let b = tokio::fs::read(&out_path).await.unwrap_or(raw_bytes);
                    let _ = tokio::fs::remove_file(&out_path).await;
                    (b, "image/jpeg")
                } else {
                    (raw_bytes, "image/png")
                };
                let _ = tokio::fs::remove_file(&tmp_path).await;

                // Get final dimensions. For JPEG, parse SOF0 marker; for PNG, use header.
                let (width, height) = if mime == "image/jpeg" {
                    jpeg_dimensions(&bytes).unwrap_or((0, 0))
                } else if bytes.len() >= 24 {
                    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
                    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
                    (w, h)
                } else {
                    (0, 0)
                };

                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

                // Return scale factor so LLM can map coordinates back.
                let scale = if width > 0 && orig_w > width { orig_w as f64 / width as f64 } else { 1.0 };

                Ok(json!({
                    "action": "screenshot",
                    "image": format!("data:{mime};base64,{b64}"),
                    "width": width,
                    "height": height,
                    "original_width": orig_w,
                    "original_height": orig_h,
                    "scale": scale
                }))
            }

            // =================================================================
            // Mouse move
            // =================================================================
            "mouse_move" => {
                let (x, y) = xy();
                if is_macos {
                    run_subprocess("cliclick", &[&format!("m:{x},{y}")]).await?;
                } else if is_windows {
                    win_set_cursor(x, y).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", &x.to_string(), &y.to_string()])
                        .await?;
                }
                Ok(json!({"action": "mouse_move", "ok": true}))
            }

            // =================================================================
            // Mouse click (left by default)
            // =================================================================
            "mouse_click" | "left_click" => {
                let (x, y) = xy();
                let button = args["button"].as_str().unwrap_or("left");
                if is_macos {
                    match button {
                        "right" => run_subprocess("cliclick", &[&format!("rc:{x},{y}")]).await?,
                        "middle" => {
                            // cliclick has no real middle-click; use CGEvent via swift
                            run_subprocess("swift", &["-e", &format!(
                                "import CoreGraphics; \
                                 let pt = CGPoint(x: {x}, y: {y}); \
                                 if let d = CGEvent(mouseEventSource: nil, mouseType: .otherMouseDown, mouseCursorPosition: pt, mouseButton: .center), \
                                    let u = CGEvent(mouseEventSource: nil, mouseType: .otherMouseUp, mouseCursorPosition: pt, mouseButton: .center) \
                                 {{ d.post(tap: .cghidEventTap); u.post(tap: .cghidEventTap) }}"
                            )]).await?;
                        }
                        _ => run_subprocess("cliclick", &[&format!("c:{x},{y}")]).await?,
                    }
                } else if is_windows {
                    win_mouse_click(x, y, button, 1).await?;
                } else {
                    let btn = match button { "right" => "3", "middle" => "2", _ => "1" };
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", btn]).await?;
                }
                Ok(json!({"action": "mouse_click", "button": button, "ok": true}))
            }

            // =================================================================
            // Double click
            // =================================================================
            "double_click" => {
                let (x, y) = xy();
                if is_macos {
                    run_subprocess("cliclick", &[&format!("dc:{x},{y}")]).await?;
                } else if is_windows {
                    win_mouse_click(x, y, "left", 2).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", "--repeat", "2", "--delay", "50", "1"]).await?;
                }
                Ok(json!({"action": "double_click", "ok": true}))
            }

            // =================================================================
            // Triple click (select whole line)
            // =================================================================
            "triple_click" => {
                let (x, y) = xy();
                if is_macos {
                    // cliclick has no tc: command; use three rapid clicks
                    let pos = format!("c:{x},{y}");
                    run_subprocess("cliclick", &[&pos, &pos, &pos]).await?;
                } else if is_windows {
                    win_mouse_click(x, y, "left", 3).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", "--repeat", "3", "--delay", "50", "1"]).await?;
                }
                Ok(json!({"action": "triple_click", "ok": true}))
            }

            // =================================================================
            // Right click
            // =================================================================
            "right_click" => {
                let (x, y) = xy();
                if is_macos {
                    run_subprocess("cliclick", &[&format!("rc:{x},{y}")]).await?;
                } else if is_windows {
                    win_mouse_click(x, y, "right", 1).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", "3"]).await?;
                }
                Ok(json!({"action": "right_click", "ok": true}))
            }

            // =================================================================
            // Middle click
            // =================================================================
            "middle_click" => {
                let (x, y) = xy();
                if is_macos {
                    // cliclick has no real middle-click; use CGEvent via swift
                    run_subprocess("swift", &["-e", &format!(
                        "import CoreGraphics; \
                         let pt = CGPoint(x: {x}, y: {y}); \
                         if let d = CGEvent(mouseEventSource: nil, mouseType: .otherMouseDown, mouseCursorPosition: pt, mouseButton: .center), \
                            let u = CGEvent(mouseEventSource: nil, mouseType: .otherMouseUp, mouseCursorPosition: pt, mouseButton: .center) \
                         {{ d.post(tap: .cghidEventTap); u.post(tap: .cghidEventTap) }}"
                    )]).await?;
                } else if is_windows {
                    win_mouse_click(x, y, "middle", 1).await?;
                } else {
                    run_subprocess("xdotool", &["mousemove", "--sync", &x.to_string(), &y.to_string(), "click", "2"]).await?;
                }
                Ok(json!({"action": "middle_click", "ok": true}))
            }

            // =================================================================
            // Drag (from x1,y1 to x2,y2)
            // =================================================================
            "drag" => {
                let x1 = args["x"].as_f64().unwrap_or(0.0) as i64;
                let y1 = args["y"].as_f64().unwrap_or(0.0) as i64;
                let x2 = args["to_x"].as_f64()
                    .ok_or_else(|| anyhow!("computer_use drag: `to_x` required"))? as i64;
                let y2 = args["to_y"].as_f64()
                    .ok_or_else(|| anyhow!("computer_use drag: `to_y` required"))? as i64;
                if is_macos {
                    run_subprocess("cliclick", &[&format!("dd:{x1},{y1}"), &format!("du:{x2},{y2}")]).await?;
                } else if is_windows {
                    run_powershell_input(&format!(
                        r#"Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinDrag {{
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern void mouse_event(uint f, uint dx, uint dy, uint d, int e);
    public static void Drag(int x1, int y1, int x2, int y2) {{
        SetCursorPos(x1, y1);
        System.Threading.Thread.Sleep(50);
        mouse_event(0x0002, 0, 0, 0, 0); // LEFTDOWN
        System.Threading.Thread.Sleep(50);
        SetCursorPos(x2, y2);
        System.Threading.Thread.Sleep(50);
        mouse_event(0x0004, 0, 0, 0, 0); // LEFTUP
    }}
}}
"@
[WinDrag]::Drag({x1}, {y1}, {x2}, {y2})"#
                    )).await?;
                } else {
                    run_subprocess("xdotool", &[
                        "mousemove", &x1.to_string(), &y1.to_string(),
                        "mousedown", "1",
                        "mousemove", "--sync", &x2.to_string(), &y2.to_string(),
                        "mouseup", "1",
                    ]).await?;
                }
                Ok(json!({"action": "drag", "from": [x1, y1], "to": [x2, y2], "ok": true}))
            }

            // =================================================================
            // Scroll (direction: up/down/left/right, amount: clicks)
            // =================================================================
            "scroll" => {
                let (x, y) = xy();
                let direction = args["direction"].as_str().unwrap_or("down");
                let amount = args["amount"].as_i64().unwrap_or(3);
                if is_macos {
                    if x != 0 || y != 0 {
                        run_subprocess("cliclick", &[&format!("m:{x},{y}")]).await?;
                    }
                    // Use CGEvent scroll via swift (macOS built-in, no deps)
                    let (scroll_y, scroll_x) = match direction {
                        "up" => (amount, 0i64),
                        "down" => (-amount, 0),
                        "left" => (0, -amount),
                        "right" => (0, amount),
                        _ => (-amount, 0),
                    };
                    run_subprocess("swift", &["-e", &format!(
                        "import CoreGraphics; \
                         if let e = CGEvent(scrollWheelEvent2Source: nil, units: .line, \
                         wheelCount: 2, wheel1: Int32({scroll_y}), wheel2: Int32({scroll_x}), wheel3: 0) \
                         {{ e.post(tap: .cghidEventTap) }}"
                    )]).await?;
                } else if is_windows {
                    if x != 0 || y != 0 {
                        win_set_cursor(x, y).await?;
                    }
                    let (wheel_flag, delta) = match direction {
                        "up" => ("0x0800", 120 * amount),    // MOUSEEVENTF_WHEEL
                        "down" => ("0x0800", -120 * amount),
                        "left" => ("0x01000", 120 * amount), // MOUSEEVENTF_HWHEEL
                        "right" => ("0x01000", -120 * amount),
                        _ => ("0x0800", -120 * amount),
                    };
                    run_powershell_input(&format!(
                        r#"Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinScroll {{
    [DllImport("user32.dll")] static extern void mouse_event(uint f, uint dx, uint dy, int d, int e);
    public static void Scroll(uint flag, int delta) {{
        mouse_event(flag, 0, 0, delta, 0);
    }}
}}
"@
[WinScroll]::Scroll({wheel_flag}, {delta})"#
                    )).await?;
                } else {
                    if x != 0 || y != 0 {
                        run_subprocess("xdotool", &["mousemove", &x.to_string(), &y.to_string()]).await?;
                    }
                    let btn = match direction {
                        "up" => "4", "down" => "5", "left" => "6", "right" => "7", _ => "5",
                    };
                    run_subprocess("xdotool", &["click", "--repeat", &amount.to_string(), "--delay", "30", btn]).await?;
                }
                Ok(json!({"action": "scroll", "direction": direction, "amount": amount, "ok": true}))
            }

            // =================================================================
            // Type text
            // =================================================================
            "type" => {
                let text = args["text"]
                    .as_str()
                    .ok_or_else(|| anyhow!("computer_use type: `text` required"))?;
                if is_macos {
                    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
                    run_subprocess(
                        "osascript",
                        &["-e", &format!("tell application \"System Events\" to keystroke \"{escaped}\"")],
                    ).await?;
                } else if is_windows {
                    // Escape SendKeys special chars: + ^ % ~ { } ( )
                    // Must handle { } carefully to avoid double-escaping
                    let mut escaped = String::with_capacity(text.len() * 2);
                    for ch in text.chars() {
                        match ch {
                            '{' => escaped.push_str("{{}"),
                            '}' => escaped.push_str("{}}"),
                            '+' => escaped.push_str("{+}"),
                            '^' => escaped.push_str("{^}"),
                            '%' => escaped.push_str("{%}"),
                            '~' => escaped.push_str("{~}"),
                            '(' => escaped.push_str("{(}"),
                            ')' => escaped.push_str("{)}"),
                            '\'' => escaped.push_str("''"),
                            other => escaped.push(other),
                        }
                    }
                    run_powershell_input(&format!(
                        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{escaped}')"
                    )).await?;
                } else {
                    run_subprocess("xdotool", &["type", "--clearmodifiers", text]).await?;
                }
                Ok(json!({"action": "type", "ok": true}))
            }

            // =================================================================
            // Key press (single key or combo like "ctrl+c")
            // =================================================================
            "key" => {
                let key = args["key"]
                    .as_str()
                    .ok_or_else(|| anyhow!("computer_use key: `key` required"))?;
                if is_macos {
                    // cliclick kp: only supports special key names (return, esc, f1, etc.)
                    // For regular characters, use t: (type). For combos, use kd:/ku: + t: or kp:.
                    if key.contains('+') {
                        let parts: Vec<&str> = key.split('+').collect();
                        let mut cliclick_args: Vec<String> = Vec::new();
                        for &modifier in &parts[..parts.len() - 1] {
                            let m = map_modifier(modifier);
                            cliclick_args.push(format!("kd:{m}"));
                        }
                        let base = parts[parts.len() - 1];
                        // Use kp: for special keys, t: for regular characters
                        if is_cliclick_special_key(base) {
                            cliclick_args.push(format!("kp:{base}"));
                        } else {
                            cliclick_args.push(format!("t:{base}"));
                        }
                        for &modifier in parts[..parts.len() - 1].iter().rev() {
                            let m = map_modifier(modifier);
                            cliclick_args.push(format!("ku:{m}"));
                        }
                        let refs: Vec<&str> = cliclick_args.iter().map(|s| s.as_str()).collect();
                        run_subprocess("cliclick", &refs).await?;
                    } else if is_cliclick_special_key(key) {
                        run_subprocess("cliclick", &[&format!("kp:{key}")]).await?;
                    } else {
                        // Single regular character — use osascript keystroke
                        let escaped = key.replace('\\', "\\\\").replace('"', "\\\"");
                        run_subprocess("osascript", &[
                            "-e", &format!("tell application \"System Events\" to keystroke \"{escaped}\""),
                        ]).await?;
                    }
                } else if is_windows {
                    let send_key = win_map_key(key);
                    let escaped = send_key.replace('\'', "''");
                    run_powershell_input(&format!(
                        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{escaped}')"
                    )).await?;
                } else {
                    run_subprocess("xdotool", &["key", key]).await?;
                }
                Ok(json!({"action": "key", "ok": true}))
            }

            // =================================================================
            // Hold key + click (e.g. Shift+Click for multi-select)
            // =================================================================
            "hold_key" => {
                let key = args["key"]
                    .as_str()
                    .ok_or_else(|| anyhow!("computer_use hold_key: `key` required"))?;
                let (x, y) = xy();
                let sub_action = args["then"].as_str().unwrap_or("click");
                if is_macos {
                    let m = map_modifier(key);
                    let click_cmd = match sub_action {
                        "double_click" => format!("dc:{x},{y}"),
                        "right_click" => format!("rc:{x},{y}"),
                        _ => format!("c:{x},{y}"),
                    };
                    run_subprocess("cliclick", &[&format!("kd:{m}"), &click_cmd, &format!("ku:{m}")]).await?;
                } else if is_windows {
                    let key_lower = key.to_lowercase();
                    let vk = match key_lower.as_str() {
                        "ctrl" | "control" => "0x11",
                        "alt" => "0x12",
                        "shift" => "0x10",
                        "win" | "super" | "cmd" | "command" => "0x5B",
                        _ => "0x10", // default to shift
                    };
                    let clicks = match sub_action { "double_click" => 2, "triple_click" => 3, _ => 1 };
                    let btn_down = match sub_action { "right_click" => "0x0008", _ => "0x0002" };
                    let btn_up = match sub_action { "right_click" => "0x0010", _ => "0x0004" };
                    run_powershell_input(&format!(
                        r#"Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinHoldKey {{
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern void mouse_event(uint f, uint dx, uint dy, uint d, int e);
    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, int extra);
    public static void HoldAndClick(int x, int y, byte vk, uint down, uint up, int clicks) {{
        keybd_event(vk, 0, 0, 0); // key down
        SetCursorPos(x, y);
        for (int i = 0; i < clicks; i++) {{
            mouse_event(down, 0, 0, 0, 0);
            mouse_event(up, 0, 0, 0, 0);
            if (i < clicks - 1) System.Threading.Thread.Sleep(50);
        }}
        keybd_event(vk, 0, 2, 0); // key up (KEYEVENTF_KEYUP=2)
    }}
}}
"@
[WinHoldKey]::HoldAndClick({x}, {y}, {vk}, {btn_down}, {btn_up}, {clicks})"#
                    )).await?;
                } else {
                    let xdo_key = map_modifier_xdotool(key);
                    run_subprocess("xdotool", &["mousemove", &x.to_string(), &y.to_string()]).await?;
                    run_subprocess("xdotool", &["keydown", &xdo_key]).await?;
                    let repeat = match sub_action { "double_click" => "2", "triple_click" => "3", _ => "1" };
                    let btn = match sub_action { "right_click" => "3", _ => "1" };
                    run_subprocess("xdotool", &["click", "--repeat", repeat, "--delay", "50", btn]).await?;
                    run_subprocess("xdotool", &["keyup", &xdo_key]).await?;
                }
                Ok(json!({"action": "hold_key", "key": key, "then": sub_action, "ok": true}))
            }

            // =================================================================
            // Cursor position — get current mouse location
            // =================================================================
            "cursor_position" => {
                let pos = if is_macos {
                    let output = tokio::process::Command::new("cliclick")
                        .arg("p:.")
                        .output()
                        .await
                        .map_err(|e| anyhow!("cliclick: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                } else if is_windows {
                    let output = powershell_hidden()
                        .args(["-Command",
                            "Add-Type -AssemblyName System.Windows.Forms; $p = [System.Windows.Forms.Cursor]::Position; \"$($p.X),$($p.Y)\""])
                        .output()
                        .await
                        .map_err(|e| anyhow!("powershell: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                } else {
                    let output = tokio::process::Command::new("xdotool")
                        .args(["getmouselocation", "--shell"])
                        .output()
                        .await
                        .map_err(|e| anyhow!("xdotool: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                };
                // Parse "x,y" format
                let parts: Vec<&str> = pos.split(',').collect();
                let (cx, cy) = if parts.len() >= 2 {
                    (
                        parts[0].trim().parse::<i64>().unwrap_or(0),
                        parts[1].trim().parse::<i64>().unwrap_or(0),
                    )
                } else {
                    // xdotool --shell format: X=123\nY=456
                    let mut cx = 0i64;
                    let mut cy = 0i64;
                    for line in pos.lines() {
                        if let Some(v) = line.strip_prefix("X=") {
                            cx = v.parse().unwrap_or(0);
                        } else if let Some(v) = line.strip_prefix("Y=") {
                            cy = v.parse().unwrap_or(0);
                        }
                    }
                    (cx, cy)
                };
                Ok(json!({"action": "cursor_position", "x": cx, "y": cy}))
            }

            // =================================================================
            // Get active window title — context awareness
            // =================================================================
            "get_active_window" => {
                let title = if is_macos {
                    let output = tokio::process::Command::new("osascript")
                        .args(["-e", "tell application \"System Events\" to get name of first process whose frontmost is true"])
                        .output()
                        .await
                        .map_err(|e| anyhow!("osascript: {e}"))?;
                    let app = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    // Also get window title
                    let output2 = tokio::process::Command::new("osascript")
                        .args(["-e", "tell application \"System Events\" to get name of front window of (first process whose frontmost is true)"])
                        .output()
                        .await;
                    let win = output2.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
                    if win.is_empty() { app } else { format!("{app} — {win}") }
                } else if is_windows {
                    let output = powershell_hidden()
                        .args(["-Command",
                            "Add-Type @\"\nusing System;\nusing System.Runtime.InteropServices;\npublic class WinTitle {\n  [DllImport(\"user32.dll\")] static extern IntPtr GetForegroundWindow();\n  [DllImport(\"user32.dll\")] static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);\n  public static string Get() {\n    var sb = new System.Text.StringBuilder(256);\n    GetWindowText(GetForegroundWindow(), sb, 256);\n    return sb.ToString();\n  }\n}\n\"@\n[WinTitle]::Get()"])
                        .output()
                        .await
                        .map_err(|e| anyhow!("powershell: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                } else {
                    let output = tokio::process::Command::new("xdotool")
                        .args(["getactivewindow", "getwindowname"])
                        .output()
                        .await
                        .map_err(|e| anyhow!("xdotool: {e}"))?;
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                };
                Ok(json!({"action": "get_active_window", "title": title}))
            }

            // =================================================================
            // UI tree — accessibility tree of the focused window
            // =================================================================
            "ui_tree" => {
                let elements_json = if is_macos {
                    let script = r#"
import Cocoa
import ApplicationServices
struct UiEl: Codable { let role: String; let label: String; let x: Int; let y: Int; let w: Int; let h: Int }
func ch(_ e: AXUIElement) -> [AXUIElement] { var v: CFTypeRef?; guard AXUIElementCopyAttributeValue(e, kAXChildrenAttribute as CFString, &v) == .success, let a = v as? [AXUIElement] else { return [] }; return a }
func a(_ e: AXUIElement, _ k: String) -> String? { var v: CFTypeRef?; guard AXUIElementCopyAttributeValue(e, k as CFString, &v) == .success, let s = v else { return nil }; return "\(s)" }
func pos(_ e: AXUIElement) -> (Int,Int)? { var v: CFTypeRef?; guard AXUIElementCopyAttributeValue(e, kAXPositionAttribute as CFString, &v) == .success, let ax = v else { return nil }; var p = CGPoint.zero; AXValueGetValue(ax as! AXValue, .cgPoint, &p); return (Int(p.x),Int(p.y)) }
func sz(_ e: AXUIElement) -> (Int,Int)? { var v: CFTypeRef?; guard AXUIElementCopyAttributeValue(e, kAXSizeAttribute as CFString, &v) == .success, let ax = v else { return nil }; var s = CGSize.zero; AXValueGetValue(ax as! AXValue, .cgSize, &s); return (Int(s.width),Int(s.height)) }
let roles: Set<String> = ["AXButton","AXTextField","AXTextArea","AXCheckBox","AXRadioButton","AXComboBox","AXPopUpButton","AXSlider","AXLink","AXMenuItem","AXMenuBarItem","AXTab","AXDisclosureTriangle","AXSearchField","AXSecureTextField","AXStaticText","AXCell"]
var r: [UiEl] = []
func walk(_ e: AXUIElement, _ d: Int) { guard d < 20, r.count < 200 else { return }; let ro = a(e, kAXRoleAttribute) ?? ""; if roles.contains(ro) { let l = a(e, kAXTitleAttribute) ?? a(e, kAXDescriptionAttribute) ?? a(e, kAXValueAttribute) ?? ""; if let (x,y) = pos(e), let (w,h) = sz(e), w > 0, h > 0 { r.append(UiEl(role: ro, label: String(l.prefix(100)), x: x, y: y, w: w, h: h)) } }; for c in ch(e) { walk(c, d+1) } }
guard let app = NSWorkspace.shared.frontmostApplication else { print("[]"); exit(0) }
let ax = AXUIElementCreateApplication(app.processIdentifier)
var wv: CFTypeRef?
if AXUIElementCopyAttributeValue(ax, kAXFocusedWindowAttribute as CFString, &wv) == .success, let w = wv { walk(w as! AXUIElement, 0) } else { walk(ax, 0) }
if let d = try? JSONEncoder().encode(r), let j = String(data: d, encoding: .utf8) { print(j) } else { print("[]") }
"#;
                    let output = tokio::process::Command::new("swift")
                        .args(["-e", script])
                        .output()
                        .await
                        .map_err(|e| anyhow!("swift ui_tree: {e}"))?;
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return Err(anyhow!("ui_tree (macos): {stderr}"));
                    }
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                } else if is_windows {
                    let ps_script = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$auto = [System.Windows.Automation.AutomationElement]::FocusedElement
$root = $null
try {
    $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
    $cur = $auto
    while ($cur -ne $null) {
        $parent = $walker.GetParent($cur)
        if ($parent -eq [System.Windows.Automation.AutomationElement]::RootElement) { $root = $cur; break }
        $cur = $parent
    }
} catch { }
if ($root -eq $null) { $root = $auto }
$results = @()
$count = 0
function Walk($el, $depth) {
    if ($depth -gt 20 -or $script:count -ge 200) { return }
    $ct = $el.Current.ControlType.ProgrammaticName
    $interactive = @('ControlType.Button','ControlType.Edit','ControlType.CheckBox','ControlType.RadioButton',
        'ControlType.ComboBox','ControlType.Slider','ControlType.Hyperlink','ControlType.MenuItem',
        'ControlType.Tab','ControlType.TabItem','ControlType.Text','ControlType.DataItem','ControlType.ListItem')
    if ($interactive -contains $ct) {
        $rect = $el.Current.BoundingRectangle
        if ($rect.Width -gt 0 -and $rect.Height -gt 0) {
            $label = $el.Current.Name
            if ([string]::IsNullOrEmpty($label)) { $label = $el.Current.AutomationId }
            $script:results += @{ role=$ct; label=$label; x=[int]$rect.X; y=[int]$rect.Y; w=[int]$rect.Width; h=[int]$rect.Height }
            $script:count++
        }
    }
    try {
        $child = $walker.GetFirstChild($el)
        while ($child -ne $null) { Walk $child ($depth+1); $child = $walker.GetNextSibling($child) }
    } catch { }
}
$walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
Walk $root 0
$results | ConvertTo-Json -Compress
"#;
                    let output = powershell_hidden()
                        .args(["-Command", ps_script])
                        .output()
                        .await
                        .map_err(|e| anyhow!("powershell ui_tree: {e}"))?;
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return Err(anyhow!("ui_tree (windows): {stderr}"));
                    }
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                } else {
                    // Linux: AT-SPI2 via python3
                    let py_script = r#"
import subprocess, json, re
out = subprocess.check_output(["gdbus", "call", "--session", "--dest=org.a11y.Bus", "--object-path=/org/a11y/bus", "--method=org.a11y.Bus.GetAddress"], text=True).strip()
# fallback: use python3-atspi if available
try:
    import gi
    gi.require_version('Atspi', '2.0')
    from gi.repository import Atspi
    desktop = Atspi.get_desktop(0)
    results = []
    interactive = {'push button','toggle button','text','password text','combo box',
                   'check box','radio button','slider','link','menu item','tab','table cell','list item'}
    def walk(el, depth):
        if depth > 20 or len(results) >= 200: return
        try:
            role = el.get_role_name()
            if role in interactive:
                c = el.get_component_iface()
                if c:
                    rect = c.get_extents(Atspi.CoordType.SCREEN)
                    if rect.width > 0 and rect.height > 0:
                        name = el.get_name() or ''
                        results.append({'role': role, 'label': name[:100], 'x': rect.x, 'y': rect.y, 'w': rect.width, 'h': rect.height})
            for i in range(el.get_child_count()):
                walk(el.get_child_at_index(i), depth + 1)
        except: pass
    # find active app
    for i in range(desktop.get_child_count()):
        app = desktop.get_child_at_index(i)
        if app:
            for j in range(app.get_child_count()):
                win = app.get_child_at_index(j)
                if win:
                    try:
                        si = win.get_state_set()
                        if si.contains(Atspi.StateType.ACTIVE):
                            walk(win, 0)
                            if results: break
                    except: pass
            if results: break
    print(json.dumps(results))
except ImportError:
    print('[]')
"#;
                    let output = tokio::process::Command::new("python3")
                        .args(["-c", py_script])
                        .output()
                        .await
                        .map_err(|e| anyhow!("python3 ui_tree: {e}"))?;
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return Err(anyhow!("ui_tree (linux): {stderr}"));
                    }
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                };
                // Parse and return as structured JSON
                let elements: Value = serde_json::from_str(&elements_json)
                    .unwrap_or_else(|_| json!([]));
                let count = elements.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(json!({"action": "ui_tree", "count": count, "elements": elements}))
            }

            // =================================================================
            // Wait — pause between actions (ms)
            // =================================================================
            "wait" => {
                let ms = args["ms"].as_u64().unwrap_or(500).min(10000); // cap at 10s
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok(json!({"action": "wait", "ms": ms, "ok": true}))
            }

            // =================================================================
            // List available desktop skills
            // =================================================================
            "list_skills" => {
                let skills_dir = crate::config::loader::base_dir()
                    .join("tools")
                    .join("computer_use")
                    .join("skills");
                let mut skills: Vec<Value> = Vec::new();
                if skills_dir.is_dir() {
                    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().is_some_and(|e| e == "md") {
                                if let Ok(content) = std::fs::read_to_string(&path) {
                                    let name = path.file_stem()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    // Extract description from frontmatter
                                    let desc = if content.starts_with("---") {
                                        content.splitn(3, "---").nth(1)
                                            .and_then(|fm| {
                                                fm.lines().find_map(|l| {
                                                    l.strip_prefix("description:")
                                                        .map(|d| d.trim().to_string())
                                                })
                                            })
                                            .unwrap_or_default()
                                    } else {
                                        String::new()
                                    };
                                    skills.push(json!({"name": name, "description": desc}));
                                }
                            }
                        }
                    }
                }
                Ok(json!({"action": "list_skills", "skills_dir": skills_dir.to_string_lossy(), "count": skills.len(), "skills": skills}))
            }

            // =================================================================
            // Get a specific desktop skill by name
            // =================================================================
            "get_skill" => {
                let name = args["name"].as_str()
                    .ok_or_else(|| anyhow!("get_skill: `name` required"))?;
                let skills_dir = crate::config::loader::base_dir()
                    .join("tools")
                    .join("computer_use")
                    .join("skills");
                let path = skills_dir.join(format!("{name}.md"));
                if !path.exists() {
                    return Err(anyhow!("skill not found: {name} (looked in {})", skills_dir.display()));
                }
                let content = std::fs::read_to_string(&path)
                    .map_err(|e| anyhow!("read skill {name}: {e}"))?;
                // Strip frontmatter, return body only
                let body = if content.starts_with("---") {
                    content.splitn(3, "---").nth(2).unwrap_or(&content).trim()
                } else {
                    content.trim()
                };
                Ok(json!({"action": "get_skill", "name": name, "content": body}))
            }

            other => Err(anyhow!(
                "computer_use: unsupported action `{other}` \
                 (supported: screenshot, mouse_move, mouse_click, double_click, triple_click, \
                 right_click, middle_click, drag, scroll, type, key, hold_key, cursor_position, \
                 get_active_window, ui_tree, list_skills, get_skill, wait)"
            )),
        }
    }
}
