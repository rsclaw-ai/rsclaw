//! Computer-use, image generation, PDF extraction, and TTS tool methods.
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
            // Wait — pause between actions (ms)
            // =================================================================
            "wait" => {
                let ms = args["ms"].as_u64().unwrap_or(500).min(10000); // cap at 10s
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok(json!({"action": "wait", "ms": ms, "ok": true}))
            }

            other => Err(anyhow!(
                "computer_use: unsupported action `{other}` \
                 (supported: screenshot, mouse_move, mouse_click, double_click, triple_click, \
                 right_click, middle_click, drag, scroll, type, key, hold_key, cursor_position, \
                 get_active_window, wait)"
            )),
        }
    }

    // -----------------------------------------------------------------------
    // New openclaw-compatible tools
    // -----------------------------------------------------------------------

    pub(crate) async fn tool_image(&self, args: Value) -> Result<Value> {
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| anyhow!("image: `prompt` required"))?;

        // Check user-configured image model: agents.defaults.model.image
        let user_image_model = self
            .handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.image.as_deref())
            .or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.image.as_deref())
            })
            .map(|s| s.to_owned());

        // Resolve provider — from image model config or current chat model
        let resolve_model = user_image_model.clone().unwrap_or_else(|| self.resolve_model_name());
        let (prov_name, user_model_id) = {
            crate::provider::registry::ProviderRegistry::parse_model(&resolve_model)
        };
        let (base_url, _auth_style) = crate::provider::defaults::resolve_base_url(prov_name);

        let default_size = match prov_name {
            _ => "2048x2048",
        };
        let size = args["size"].as_str().unwrap_or(default_size);

        // Also check provider config for api_key and base_url overrides
        let cfg_key = self
            .config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get(prov_name))
            .and_then(|p| p.api_key.as_ref())
            .and_then(|k| k.as_plain().map(str::to_owned));
        let cfg_url = self
            .config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get(prov_name))
            .and_then(|p| p.base_url.clone());

        // Providers with image generation support
        let image_providers = ["doubao", "bytedance", "openai", "qwen", "minimax", "gemini"];
        let (img_url, img_key, img_prov) = if image_providers.contains(&prov_name) {
            let url = cfg_url.unwrap_or(base_url);
            let key = cfg_key
                .or_else(|| std::env::var(format!("{}_API_KEY", prov_name.to_uppercase())).ok())
                .or_else(|| std::env::var("OPENAI_API_KEY").ok());
            (url, key, prov_name)
        } else {
            // Current provider doesn't support images — try doubao, qwen, openai
            let fallback = [("doubao", "ARK_API_KEY"), ("qwen", "DASHSCOPE_API_KEY"), ("minimax", "MINIMAX_API_KEY"), ("gemini", "GEMINI_API_KEY"), ("openai", "OPENAI_API_KEY")];
            let mut found = None;
            for (fb_prov, fb_env) in fallback {
                let fb_cfg = self
                    .config
                    .model
                    .models
                    .as_ref()
                    .and_then(|m| m.providers.get(fb_prov));
                let fb_key = fb_cfg
                    .and_then(|p| p.api_key.as_ref())
                    .and_then(|k| k.as_plain().map(str::to_owned))
                    .or_else(|| std::env::var(fb_env).ok());
                if let Some(key) = fb_key {
                    let fb_url = fb_cfg
                        .and_then(|p| p.base_url.clone())
                        .unwrap_or_else(|| crate::provider::defaults::resolve_base_url(fb_prov).0);
                    found = Some((fb_url, Some(key), fb_prov));
                    break;
                }
            }
            found.unwrap_or_else(|| (cfg_url.unwrap_or(base_url), None, prov_name))
        };
        let Some(api_key) = img_key else {
            return Ok(json!({
                "error": "AI image generation requires doubao, qwen, minimax, gemini, or openai provider with API key. No image-capable provider configured."
            }));
        };

        let image_model = args["model"].as_str()
            .or_else(|| if !user_model_id.is_empty() { Some(user_model_id) } else { None })
            .unwrap_or_else(|| match img_prov {
                "doubao" | "bytedance" => "doubao-seedream-5-0-260128",
                "openai" => "dall-e-3",
                "qwen" => "qwen-image-2.0-pro",
                "minimax" => "image-01",
                "gemini" => "gemini-3-pro-image-preview",
                _ => "dall-e-3",
            });

        // Resolve User-Agent: provider config → gateway config → default
        let img_ua = self.config.model.models.as_ref()
            .and_then(|m| m.providers.get(img_prov))
            .and_then(|p| p.user_agent.as_deref())
            .or_else(|| self.config.gateway.user_agent.as_deref())
            .unwrap_or(crate::provider::DEFAULT_USER_AGENT);
        let client = reqwest::Client::builder()
            .user_agent(img_ua)
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default();

        tracing::info!(provider = img_prov, model = image_model, size = size, ua = img_ua, "tool_image: generating");

        // Provider-specific API formats
        let is_qwen = img_prov == "qwen";
        let is_minimax = img_prov == "minimax";
        let is_gemini = img_prov == "gemini";
        let (resp_status, resp_body) = if is_qwen {
            let qwen_size = size.replace('x', "*");
            let resp = client
                .post("https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation")
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({
                    "model": image_model,
                    "input": { "messages": [{ "role": "user", "content": [{ "text": prompt }] }] },
                    "parameters": { "size": qwen_size, "n": 1, "watermark": false }
                }))
                .send().await
                .map_err(|e| anyhow!("image: request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| anyhow!("image: parse error: {e}"))?;
            (st, body)
        } else if is_minimax {
            // Minimax: /v1/image_generation, aspect_ratio instead of size
            // Supported: "1:1", "16:9", "9:16", "4:3", "3:4", "2:3", "3:2"
            let aspect = if size.contains('x') {
                let parts: Vec<&str> = size.split('x').collect();
                if parts.len() == 2 {
                    let w = parts[0].parse::<f32>().unwrap_or(1024.0);
                    let h = parts[1].parse::<f32>().unwrap_or(1024.0);
                    let ratio = w / h.max(1.0);
                    let candidates = [
                        (1.0_f32, "1:1"),
                        (16.0 / 9.0, "16:9"),
                        (9.0 / 16.0, "9:16"),
                        (4.0 / 3.0, "4:3"),
                        (3.0 / 4.0, "3:4"),
                        (3.0 / 2.0, "3:2"),
                        (2.0 / 3.0, "2:3"),
                    ];
                    candidates
                        .iter()
                        .min_by(|a, b| {
                            (a.0 - ratio)
                                .abs()
                                .partial_cmp(&(b.0 - ratio).abs())
                                .unwrap()
                        })
                        .map(|c| c.1)
                        .unwrap_or("1:1")
                        .to_owned()
                } else {
                    "1:1".to_owned()
                }
            } else {
                "1:1".to_owned()
            };
            let url = format!("{}/image_generation", img_url.trim_end_matches('/'));
            let resp = client.post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({ "model": image_model, "prompt": prompt, "aspect_ratio": aspect, "response_format": "url" }))
                .send().await
                .map_err(|e| anyhow!("image: request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| anyhow!("image: parse error: {e}"))?;
            (st, body)
        } else if is_gemini {
            // Gemini: generateContent with responseModalities: ["IMAGE"]
            // Map size to aspect ratio for Gemini
            let aspect = if size.contains('x') {
                let parts: Vec<&str> = size.split('x').collect();
                if parts.len() == 2 {
                    let w = parts[0].parse::<u32>().unwrap_or(2048);
                    let h = parts[1].parse::<u32>().unwrap_or(2048);
                    if w == h { "1:1" } else if w > h { "16:9" } else { "9:16" }
                } else { "1:1" }
            } else { "1:1" };
            let gemini_base = img_url.trim_end_matches('/');
            let url = format!("{gemini_base}/models/{image_model}:generateContent?key={api_key}");
            let resp = client.post(&url)
                .json(&json!({
                    "contents": [{ "parts": [{ "text": prompt }] }],
                    "generationConfig": {
                        "responseModalities": ["TEXT", "IMAGE"],
                        "imageConfig": { "aspectRatio": aspect }
                    }
                }))
                .send().await
                .map_err(|e| anyhow!("image: gemini request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp.json().await.map_err(|e| anyhow!("image: gemini parse error: {e}"))?;
            (st, body)
        } else {
            let url = format!("{}/images/generations", img_url.trim_end_matches('/'));
            let resp = client.post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({ "model": image_model, "prompt": prompt, "size": size, "n": 1, "response_format": "url" }))
                .send().await
                .map_err(|e| anyhow!("image: request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| anyhow!("image: parse error: {e}"))?;
            (st, body)
        };

        if !resp_status.is_success() {
            let err_msg = resp_body["error"]["message"]
                .as_str()
                .or_else(|| resp_body["message"].as_str())
                .unwrap_or("unknown error");
            return Err(anyhow!("image: API error: {err_msg}"));
        }

        // Extract image URL/base64 — different response formats per provider
        // Gemini returns inline base64 directly, others return URLs
        if is_gemini {
            // Gemini: candidates[0].content.parts[] — find the inlineData part
            #[allow(unused_imports)]
            use base64::Engine;
            let parts = resp_body.pointer("/candidates/0/content/parts")
                .and_then(|v| v.as_array());
            if let Some(parts) = parts {
                for part in parts {
                    if let Some(inline) = part.get("inlineData") {
                        let mime = inline.get("mimeType").and_then(|v| v.as_str()).unwrap_or("image/png");
                        if let Some(b64_data) = inline.get("data").and_then(|v| v.as_str()) {
                            let data_uri = format!("data:{mime};base64,{b64_data}");
                            return Ok(json!({
                                "url": data_uri,
                                "revised_prompt": prompt
                            }));
                        }
                    }
                }
            }
            return Err(anyhow!("image: no image data in Gemini response"));
        }

        let img_url_str = if is_qwen {
            resp_body
                .pointer("/output/choices/0/message/content/0/image")
                .and_then(|v| v.as_str())
        } else if is_minimax {
            // minimax: data.image_base64[0] (base64) or data.image_urls[0] (url)
            resp_body.pointer("/data/image_urls/0").and_then(|v| v.as_str())
                .or_else(|| resp_body.pointer("/data/image_base64/0").and_then(|v| v.as_str()))
        } else {
            resp_body.pointer("/data/0/url").and_then(|v| v.as_str())
        };

        let Some(img_url_str) = img_url_str else {
            return Err(anyhow!("image: no image URL in response"));
        };

        // Download image and convert to data URI
        use base64::Engine;
        let image_result = match reqwest::Client::new()
            .get(img_url_str)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(bytes) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    format!("data:image/png;base64,{b64}")
                }
                Err(e) => return Err(anyhow!("image: download failed: {e}")),
            },
            Ok(r) => return Err(anyhow!("image: download returned {}", r.status())),
            Err(e) => return Err(anyhow!("image: download error: {e}")),
        };

        let revised = resp_body
            .pointer("/data/0/revised_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        Ok(json!({
            "url": image_result,
            "revised_prompt": revised,
            "size": size,
            "model": image_model
        }))
    }

    pub(crate) async fn tool_pdf(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("pdf: `path` required"))?;

        // If URL, download to temp file first.
        let local_path = if path.starts_with("http://") || path.starts_with("https://") {
            let tmp = std::env::temp_dir().join("rsclaw_pdf_download.pdf");
            let client = reqwest::Client::new();
            let bytes = client
                .get(path)
                .send()
                .await
                .map_err(|e| anyhow!("pdf: download failed: {e}"))?
                .bytes()
                .await
                .map_err(|e| anyhow!("pdf: download read failed: {e}"))?;
            tokio::fs::write(&tmp, &bytes)
                .await
                .map_err(|e| anyhow!("pdf: write temp file failed: {e}"))?;
            tmp
        } else {
            std::path::PathBuf::from(path)
        };

        // Pure Rust PDF extraction, with pdftotext CLI fallback.
        let pdf_bytes = tokio::fs::read(&local_path)
            .await
            .map_err(|e| anyhow!("pdf: read failed: {e}"))?;
        let text = match crate::agent::doc::safe_extract_pdf_from_mem(&pdf_bytes) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("pdf-extract failed ({e}), trying pdftotext CLI");
                let output = tokio::process::Command::new("pdftotext")
                    .args([local_path.to_str().unwrap_or(""), "-"])
                    .output()
                    .await
                    .map_err(|e2| anyhow!("pdf: extraction failed: {e}, pdftotext: {e2}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(anyhow!("pdf: extraction failed: {e}, pdftotext: {stderr}"));
                }
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
        };
        // Truncate to 100k chars to avoid blowing up context.
        let truncated = if text.len() > 100_000 {
            let mut end = 100_000usize;
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
            format!("{}...\n[truncated at 100000 chars]", &text[..end])
        } else {
            text
        };

        Ok(json!({
            "path": path,
            "text": truncated,
            "chars": truncated.len()
        }))
    }

    /// Generate TTS audio from text. Prefers sherpa-onnx, falls back to system TTS.
    /// Returns the path to the generated audio file.
    pub(crate) async fn generate_tts_audio(&self, text: &str) -> Result<String> {
        // Truncate long text for TTS (avoid very long audio).
        let tts_text = if text.chars().count() > 500 {
            let idx = text.char_indices().nth(500).map(|(i, _)| i).unwrap_or(text.len());
            &text[..idx]
        } else {
            text
        };

        let out_path = std::env::temp_dir().join(format!(
            "rsclaw_tts_{}.wav",
            chrono::Utc::now().timestamp_millis()
        ));
        let out_str = out_path.to_string_lossy().to_string();

        // Try sherpa-onnx first (installed via `rsclaw tools install sherpa-onnx`).
        let sherpa_bin = crate::config::loader::base_dir()
            .join("tools")
            .join("sherpa-onnx")
            .join("bin")
            .join(if cfg!(target_os = "windows") { "sherpa-onnx-offline-tts.exe" } else { "sherpa-onnx-offline-tts" });

        if sherpa_bin.exists() {
            let model_dir = crate::config::loader::base_dir()
                .join("tools")
                .join("sherpa-onnx")
                .join("models")
                .join("tts");
            // Look for any VITS model config.
            let model_config = model_dir.join("model.onnx");
            if model_config.exists() {
                let mut cmd = tokio::process::Command::new(&sherpa_bin);
                cmd.args([
                    "--vits-model", model_config.to_str().unwrap_or(""),
                    "--vits-tokens", model_dir.join("tokens.txt").to_str().unwrap_or(""),
                    "--output-filename", &out_str,
                    "--vits-length-scale", "1.0",
                    tts_text,
                ]);
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                let output = cmd.output().await;
                if let Ok(o) = output {
                    if o.status.success() && out_path.exists() {
                        return Ok(out_str);
                    }
                }
                // Fall through to system TTS if sherpa-onnx failed.
            }
        }

        // Fallback: system TTS (same as tool_tts).
        #[cfg(target_os = "macos")]
        {
            let output = tokio::process::Command::new("say")
                .args(["-o", &out_str, tts_text])
                .output()
                .await
                .map_err(|e| anyhow!("auto-tts: say failed: {e}"))?;
            if !output.status.success() {
                return Err(anyhow!("auto-tts: say exit code {}", output.status));
            }
        }
        #[cfg(target_os = "windows")]
        {
            let safe_text = tts_text.replace('\'', "''");
            let script = format!(
                "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; $s.SetOutputToWaveFile('{}'); $s.Speak('{}')",
                out_str.replace('\'', "''"), safe_text
            );
            let output = powershell_hidden()
                .args(["-Command", &script])
                .output()
                .await
                .map_err(|e| anyhow!("auto-tts: SAPI failed: {e}"))?;
            if !output.status.success() {
                return Err(anyhow!("auto-tts: SAPI exit code {}", output.status));
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let result = tokio::process::Command::new("espeak")
                .args(["-w", &out_str, tts_text])
                .output()
                .await;
            match result {
                Ok(o) if o.status.success() => {}
                _ => {
                    tokio::process::Command::new("pico2wave")
                        .args(["-w", &out_str, "--", tts_text])
                        .output()
                        .await
                        .map_err(|e| anyhow!("auto-tts: no TTS engine available: {e}"))?;
                }
            }
        }

        if out_path.exists() {
            Ok(out_str)
        } else {
            Err(anyhow!("auto-tts: output file not created"))
        }
    }

    pub(crate) async fn tool_tts(&self, args: Value) -> Result<Value> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("tts: `text` required"))?;
        let voice = args["voice"].as_str().unwrap_or("default");

        let out_path = std::env::temp_dir().join(format!(
            "rsclaw_tts_{}{}",
            chrono::Utc::now().timestamp_millis(),
            if cfg!(target_os = "windows") {
                ".wav"
            } else {
                ".aiff"
            }
        ));
        let out_path_str = out_path.to_string_lossy().to_string();

        let is_macos = cfg!(target_os = "macos");
        let is_windows = cfg!(target_os = "windows");

        if is_macos {
            let mut cmd = tokio::process::Command::new("say");
            if voice != "default" {
                cmd.args(["-v", voice]);
            }
            cmd.args(["-o", &out_path_str, text]);
            let output = cmd
                .output()
                .await
                .map_err(|e| anyhow!("tts: `say` command failed: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow!("tts: say failed: {stderr}"));
            }
        } else if is_windows {
            let script = format!(
                r#"
Add-Type -AssemblyName System.Speech
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
$synth.SetOutputToWaveFile('{}')
$synth.Speak('{}')
"#,
                out_path_str, text
            );
            let output = powershell_hidden()
                .args(["-Command", &script])
                .output()
                .await
                .map_err(|e| anyhow!("tts: PowerShell SAPI failed: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow!("tts: SAPI failed: {stderr}"));
            }
        } else {
            let espeak_result = tokio::process::Command::new("espeak")
                .args(["-w", &out_path_str, text])
                .output()
                .await;
            match espeak_result {
                Ok(o) if o.status.success() => {}
                _ => {
                    let output = tokio::process::Command::new("pico2wave")
                        .args(["-w", &out_path_str, "--", text])
                        .output()
                        .await
                        .map_err(|e| anyhow!("tts: neither espeak nor pico2wave available: {e}"))?;
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return Err(anyhow!("tts: pico2wave failed: {stderr}"));
                    }
                }
            }
        }

        Ok(json!({
            "audio_file": out_path_str,
            "voice": voice,
            "chars": text.len()
        }))
    }
}
