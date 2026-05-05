//! Computer-use tool method (screen capture, mouse, keyboard, window management).
//!
//! Split from `runtime.rs` to reduce file size.  All methods live in
//! `impl AgentRuntime` via the split-impl pattern (same struct, different file).

use anyhow::{Result, anyhow};
use base64::Engine;
use serde_json::{Value, json};

use super::platform::{
    display_logical_scale, jpeg_dimensions, is_cliclick_special_key, map_modifier,
    map_modifier_xdotool, powershell_hidden, run_powershell_input, run_subprocess, win_map_key,
    win_mouse_click, win_set_cursor,
};

/// System prompt for UI-TARS end-to-end agent loop.
/// Matches the training data format exactly for reproducible VLM outputs.
const UI_TARS_SYSTEM_PROMPT: &str = r#"You are a GUI agent. You are given a task and your action history, with screenshots. You need to perform the next action to complete the task.

## Output Format
```
Thought: ...
Action: ...
```

## Action Space

click(start_box='<|box_start|>(x1, y1)<|box_end|>')
left_double(start_box='<|box_start|>(x1, y1)<|box_end|>')
right_single(start_box='<|box_start|>(x1, y1)<|box_end|>')
drag(start_box='<|box_start|>(x1, y1)<|box_end|>', end_box='<|box_start|>(x3, y3)<|box_end|>')
hotkey(key='')
type(content='') #If you want to submit your input, use "\n" at the end of `content`.
scroll(start_box='<|box_start|>(x1, y1)<|box_end|>', direction='down or up or right or left')
wait() #Sleep for 5s and take a screenshot to check for any changes.
finished(content='xxx') # Use escape characters \', \", and \n in content part to ensure we can parse the content in normal python string format.

## Note
- Use Chinese in `Thought` part.
- Write a small plan and finally summarize your next action (with its target element) in one sentence in `Thought` part.
"#;

impl super::runtime::AgentRuntime {
    /// Capture a screenshot and return the saved image path + metadata.
    /// Extracted from `tool_computer_use` so `ui_tars` can call it without
    /// recursion (async fn recursion requires boxing).
    async fn tool_screenshot(
        &self,
        region: Option<(f64, f64, f64, f64)>,
        max_long_edge_px: Option<u32>,
        quality: Option<u32>,
    ) -> Result<Value> {
        let is_macos = cfg!(target_os = "macos");
        let is_windows = cfg!(target_os = "windows");

        let tmp_path = std::env::temp_dir().join("rsclaw_screen.png");
        let tmp_path_str = tmp_path.to_string_lossy().to_string();

        let output = if is_macos {
            let mut cmd = tokio::process::Command::new("screencapture");
            cmd.arg("-x");
            if let Some((rx, ry, rw, rh)) = region {
                let scale = display_logical_scale();
                let lx = (rx / scale).round() as i64;
                let ly = (ry / scale).round() as i64;
                let lw = (rw / scale).round().max(1.0) as i64;
                let lh = (rh / scale).round().max(1.0) as i64;
                cmd.args(["-R", &format!("{lx},{ly},{lw},{lh}")]);
            }
            cmd.arg(&tmp_path_str).output().await
        } else if is_windows {
            let (rx, ry, rw, rh) = region
                .map(|(x, y, w, h)| (x as i64, y as i64, w as i64, h as i64))
                .unwrap_or((-1, -1, -1, -1));
            let region_init = if rw > 0 {
                format!("$rx={rx}; $ry={ry}; $rw={rw}; $rh={rh};")
            } else {
                "$rx=-1; $ry=-1; $rw=-1; $rh=-1;".to_owned()
            };
            let script = format!(
                r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
{region_init}
if ($rw -gt 0) {{
    $bounds = New-Object System.Drawing.Rectangle($rx, $ry, $rw, $rh)
}} else {{
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
}}
$bitmap = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bitmap.Save('{tmp_path_str}')
$graphics.Dispose()
$bitmap.Dispose()
"#
            );
            powershell_hidden()
                .args(["-Command", &script])
                .output()
                .await
        } else {
            let res = if let Some((rx, ry, rw, rh)) = region {
                let area = format!(
                    "{x},{y},{w},{h}",
                    x = rx as i64,
                    y = ry as i64,
                    w = rw as i64,
                    h = rh as i64
                );
                tokio::process::Command::new("scrot")
                    .args(["-a", &area, &tmp_path_str])
                    .output()
                    .await
            } else {
                tokio::process::Command::new("scrot")
                    .arg(&tmp_path_str)
                    .output()
                    .await
            };
            if !matches!(&res, Ok(o) if o.status.success()) {
                let mut cmd = tokio::process::Command::new("import");
                cmd.args(["-window", "root"]);
                if let Some((rx, ry, rw, rh)) = region {
                    cmd.args(["-crop", &format!(
                        "{w}x{h}+{x}+{y}",
                        x = rx as i64,
                        y = ry as i64,
                        w = rw as i64,
                        h = rh as i64
                    )]);
                }
                cmd.arg(&tmp_path_str).output().await
            } else {
                res
            }
        }
        .map_err(|e| anyhow!("computer_use screenshot: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("computer_use screenshot failed: {stderr}"));
        }

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

        const DEFAULT_MAX_LONG_EDGE: u32 = 1024;
        // quality: None = default 30, Some(0) = keep PNG, Some(q) = JPEG quality 1-100
        let jpg_quality = quality.unwrap_or(30).clamp(0, 100);
        let keep_png = jpg_quality == 0;
        let max_long_edge = max_long_edge_px
            .filter(|n| *n >= 64 && *n <= 8192)
            .unwrap_or(DEFAULT_MAX_LONG_EDGE);
        let long_edge = orig_w.max(orig_h);
        let need_resize = long_edge > max_long_edge;
        let max_long_edge_str = max_long_edge.to_string();

        let out_path = if keep_png {
            std::env::temp_dir().join("rsclaw_screen_out.png")
        } else {
            std::env::temp_dir().join("rsclaw_screen_out.jpg")
        };
        let out_str = out_path.to_string_lossy().to_string();

        let converted = if is_macos {
            if keep_png && !need_resize {
                // No conversion needed, use raw PNG directly
                false
            } else {
                let quality_str = jpg_quality.to_string();
                let mut sips_args: Vec<&str> = vec![];
                if need_resize {
                    sips_args.extend_from_slice(&["-Z", &max_long_edge_str]);
                }
                if keep_png {
                    sips_args.extend_from_slice(&[
                        "-s", "format", "png",
                        &tmp_path_str,
                        "--out", &out_str,
                    ]);
                } else {
                    sips_args.extend_from_slice(&[
                        "-s", "format", "jpeg",
                        "-s", "formatOptions", &quality_str,
                        &tmp_path_str,
                        "--out", &out_str,
                    ]);
                }
                tokio::process::Command::new("sips")
                    .args(&sips_args)
                    .output()
                    .await
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            }
        } else if is_windows {
            let (new_w, new_h) = if need_resize {
                let ratio = max_long_edge as f64 / long_edge as f64;
                (
                    ((orig_w as f64) * ratio).round().max(1.0) as u32,
                    ((orig_h as f64) * ratio).round().max(1.0) as u32,
                )
            } else {
                (orig_w, orig_h)
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
$params.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter([System.Drawing.Imaging.Encoder]::Quality, [long]{jpg_quality})
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
            let mut convert_args: Vec<&str> = vec![&tmp_path_str];
            let resize_box = format!("{m}x{m}>", m = max_long_edge);
            if need_resize {
                convert_args.extend_from_slice(&["-resize", &resize_box]);
            }
            let quality_str = if keep_png { "100".to_string() } else { jpg_quality.to_string() };
            convert_args.extend_from_slice(&["-quality", &quality_str, &out_str]);
            tokio::process::Command::new("convert")
                .args(&convert_args)
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false)
        };

        let (bytes, mime) = if converted {
            let b = tokio::fs::read(&out_path).await.unwrap_or(raw_bytes);
            let _ = tokio::fs::remove_file(&out_path).await;
            if keep_png {
                (b, "image/png")
            } else {
                (b, "image/jpeg")
            }
        } else {
            (raw_bytes, "image/png")
        };
        let _ = tokio::fs::remove_file(&tmp_path).await;

        let (width, height) = if mime == "image/jpeg" {
            jpeg_dimensions(&bytes).unwrap_or((0, 0))
        } else if bytes.len() >= 24 {
            let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
            let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
            (w, h)
        } else {
            (0, 0)
        };

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let ext = if mime == "image/jpeg" { "jpg" } else { "png" };
        let save_dir = dirs_next::download_dir()
            .unwrap_or_else(|| {
                dirs_next::home_dir()
                    .unwrap_or_else(crate::config::loader::base_dir)
                    .join("Downloads")
            })
            .join("rsclaw")
            .join("screenshots");
        tokio::fs::create_dir_all(&save_dir)
            .await
            .map_err(|e| anyhow!("computer_use screenshot: create_dir: {e}"))?;
        let save_path = save_dir.join(format!("{nanos:x}.{ext}"));
        tokio::fs::write(&save_path, &bytes)
            .await
            .map_err(|e| anyhow!("computer_use screenshot: write: {e}"))?;

        let scale = if width > 0 && orig_w > width { orig_w as f64 / width as f64 } else { 1.0 };

        Ok(json!({
            "action": "screenshot",
            "image_path": save_path.to_string_lossy(),
            "mime": mime,
            "width": width,
            "height": height,
            "original_width": orig_w,
            "original_height": orig_h,
            "scale": scale
        }))
    }

    /// Infer the target macOS application name from a UI-TARS instruction.
    /// Returns the canonical app name for `osascript tell application`.
    fn extract_app_name(instruction: &str) -> Option<&'static str> {
        let lower = instruction.to_lowercase();
        if lower.contains("wechat") || instruction.contains("微信") {
            return Some("WeChat");
        }
        if lower.contains("telegram") {
            return Some("Telegram");
        }
        if lower.contains("safari") {
            return Some("Safari");
        }
        if lower.contains("google chrome") || lower.contains("chrome") {
            return Some("Google Chrome");
        }
        if lower.contains("finder") {
            return Some("Finder");
        }
        if lower.contains("slack") {
            return Some("Slack");
        }
        if lower.contains("notes") || lower.contains("备忘录") {
            return Some("Notes");
        }
        None
    }

    pub(crate) async fn tool_computer_use(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("computer_use: `action` required"))?;

        let is_macos = cfg!(target_os = "macos");
        let is_windows = cfg!(target_os = "windows");

        // Helper: extract x, y from args (physical pixels — same coordinate
        // space as the screenshot's `original_width`/`original_height`).
        let xy_physical = || {
            (
                args["x"].as_f64().unwrap_or(0.0),
                args["y"].as_f64().unwrap_or(0.0),
            )
        };

        // Helper: convert physical-pixel coords to whatever the platform's
        // input simulator expects.
        //   macOS (cliclick / screencapture -R): logical point coordinates
        //     — divide by the display backing-scale factor.
        //   Windows / Linux: physical pixels — pass through unchanged.
        // Returns `(x, y)` as `i64` rounded to the nearest integer.
        let xy = || {
            let (px, py) = xy_physical();
            let scale = if is_macos { display_logical_scale() } else { 1.0 };
            ((px / scale).round() as i64, (py / scale).round() as i64)
        };
        let to_native = |px: f64, py: f64| -> (i64, i64) {
            let scale = if is_macos { display_logical_scale() } else { 1.0 };
            ((px / scale).round() as i64, (py / scale).round() as i64)
        };

        match action {
            // =================================================================
            // Screenshot — capture + auto-resize for HiDPI (saves tokens)
            // =================================================================
            "screenshot" => {
                let region = args.get("region").and_then(|v| {
                    let x = v.get("x")?.as_f64()?;
                    let y = v.get("y")?.as_f64()?;
                    let w = v.get("width")?.as_f64()?;
                    let h = v.get("height")?.as_f64()?;
                    if w <= 0.0 || h <= 0.0 {
                        return None;
                    }
                    Some((x, y, w, h))
                });
                let max_long_edge = args["max_long_edge_px"].as_u64().map(|n| n as u32);
                self.tool_screenshot(region, max_long_edge, None).await
            }

            // =================================================================
            // Mouse move
            // =================================================================
            "mouse_move" => {
                let (x, y) = xy();
                tracing::info!(action = "mouse_move", x, y, is_macos, "computer_use cliclick");
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
                tracing::info!(action = "mouse_click", x, y, button, is_macos, "computer_use cliclick");
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
                tracing::info!(action = "double_click", x, y, is_macos, "computer_use cliclick");
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
                tracing::info!(action = "triple_click", x, y, is_macos, "computer_use cliclick");
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
                tracing::info!(action = "right_click", x, y, is_macos, "computer_use cliclick");
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
                let (x1, y1) = xy();
                let to_x_phys = args["to_x"].as_f64()
                    .ok_or_else(|| anyhow!("computer_use drag: `to_x` required"))?;
                let to_y_phys = args["to_y"].as_f64()
                    .ok_or_else(|| anyhow!("computer_use drag: `to_y` required"))?;
                let (x2, y2) = to_native(to_x_phys, to_y_phys);
                tracing::info!(action = "drag", x1, y1, x2, y2, is_macos, "computer_use cliclick");
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
                    // AppleScript `keystroke` with non-ASCII (CJK / emoji / etc.)
                    // is unreliable — the active input method swallows or
                    // mangles the characters, often dropping them or replacing
                    // with Latin fallback. Route those through the clipboard +
                    // cmd-V instead, which is layout-independent. Pure ASCII
                    // still uses keystroke for speed and clipboard-preserving.
                    if text.is_ascii() {
                        run_subprocess(
                            "osascript",
                            &["-e", &format!("tell application \"System Events\" to keystroke \"{escaped}\"")],
                        ).await?;
                    } else {
                        let script = format!(
                            "set the clipboard to \"{escaped}\"\n\
                             delay 0.2\n\
                             tell application \"System Events\" to keystroke \"v\" using command down"
                        );
                        run_subprocess("osascript", &["-e", &script]).await?;
                    }
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
                // Normalize UI-TARS key names (e.g. "pagedown" → "page-down") so they
                // match cliclick's special-key vocabulary instead of being typed as text.
                let key = normalize_key_name(key);
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
                            cliclick_args.push(format!("kp:{}", base.to_lowercase()));
                        } else {
                            cliclick_args.push(format!("t:{base}"));
                        }
                        for &modifier in parts[..parts.len() - 1].iter().rev() {
                            let m = map_modifier(modifier);
                            cliclick_args.push(format!("ku:{m}"));
                        }
                        let refs: Vec<&str> = cliclick_args.iter().map(|s| s.as_str()).collect();
                        run_subprocess("cliclick", &refs).await?;
                    } else if key.eq_ignore_ascii_case("return") || key.eq_ignore_ascii_case("enter") {
                        // AppleScript key code 36 is more reliable than cliclick kp:return
                        // for apps like WeChat whose input box may not respond to cliclick's CGEvent.
                        run_subprocess("osascript", &[
                            "-e", "tell application \"System Events\" to key code 36",
                        ]).await?;
                    } else if is_cliclick_special_key(&key) {
                        run_subprocess("cliclick", &[&format!("kp:{}", key.to_lowercase())]).await?;
                    } else {
                        // Single regular character — use osascript keystroke
                        let escaped = key.replace('\\', "\\\\").replace('"', "\\\"");
                        run_subprocess("osascript", &[
                            "-e", &format!("tell application \"System Events\" to keystroke \"{escaped}\""),
                        ]).await?;
                    }
                } else if is_windows {
                    let send_key = win_map_key(&key);
                    let escaped = send_key.replace('\'', "''");
                    run_powershell_input(&format!(
                        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{escaped}')"
                    )).await?;
                } else {
                    run_subprocess("xdotool", &["key", &key]).await?;
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
            // List available app-rules (per-app desktop automation playbooks)
            // =================================================================
            "list_app_rules" | "list_skills" => {
                // `list_skills` is the legacy alias from before the
                // skills/ → app-rules/ rename; kept for prompts that
                // already learned the old name.
                let app_rules_dir = crate::config::loader::base_dir()
                    .join("tools")
                    .join("computer_use")
                    .join("app-rules");
                let mut rules: Vec<Value> = Vec::new();
                if app_rules_dir.is_dir() {
                    if let Ok(entries) = std::fs::read_dir(&app_rules_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().is_some_and(|e| e == "md") {
                                if let Ok(content) = std::fs::read_to_string(&path) {
                                    let name = path.file_stem()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    // Extract description from frontmatter
                                    let desc = if content.starts_with("---") {
                                        content.split("---").nth(1)
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
                                    rules.push(json!({"name": name, "description": desc}));
                                }
                            }
                        }
                    }
                }
                Ok(json!({
                    "action": "list_app_rules",
                    "app_rules_dir": app_rules_dir.to_string_lossy(),
                    "count": rules.len(),
                    "app_rules": rules,
                }))
            }

            // =================================================================
            // Get a specific app-rule by name
            // =================================================================
            "get_app_rule" | "get_skill" => {
                let name = args["name"].as_str()
                    .ok_or_else(|| anyhow!("get_app_rule: `name` required"))?;
                let app_rules_dir = crate::config::loader::base_dir()
                    .join("tools")
                    .join("computer_use")
                    .join("app-rules");
                let path = app_rules_dir.join(format!("{name}.md"));
                if !path.exists() {
                    return Err(anyhow!(
                        "app-rule not found: {name} (looked in {})",
                        app_rules_dir.display()
                    ));
                }
                let content = std::fs::read_to_string(&path)
                    .map_err(|e| anyhow!("read app-rule {name}: {e}"))?;
                // Strip frontmatter, return body only
                let body = if content.starts_with("---") {
                    content.splitn(3, "---").nth(2).unwrap_or(&content).trim()
                } else {
                    content.trim()
                };
                Ok(json!({"action": "get_app_rule", "name": name, "content": body}))
            }

            // =================================================================
            // UI-TARS end-to-end agent loop — screenshot → predict → execute
            // =================================================================
            "ui_tars" => {
                let instruction = args["instruction"]
                    .as_str()
                    .ok_or_else(|| anyhow!("computer_use ui_tars: `instruction` required"))?;
                let max_steps = args["max_steps"].as_u64().unwrap_or(30) as usize;

                // Read config for UI-TARS API URL, key, and model.
                let (api_url, api_key, model) = self
                    .config
                    .raw
                    .tools
                    .as_ref()
                    .and_then(|t| t.computer_use.as_ref())
                    .map(|cu| {
                        (
                            cu.ui_analyze_api_url.clone(),
                            cu.ui_analyze_api_key.clone(),
                            cu.ui_analyze_model.clone(),
                        )
                    })
                    .unwrap_or((None, None, None));

                let Some(api_url) = api_url else {
                    return Err(anyhow!(
                        "computer_use ui_tars: ui_analyze_api_url not configured. \
                         Set tools.computerUse.uiAnalyzeApiUrl in config."
                    ));
                };

                let mut provider = crate::provider::ui_tars::UiTarsProvider::new(api_url, api_key)
                    .with_version(crate::provider::ui_tars::UiTarsVersion::V1_5);
                if let Some(model) = model {
                    provider = provider.with_model(model);
                }

                let mut history: Vec<(String, String)> = Vec::new();
                let mut steps: Vec<Value> = Vec::new();
                // Track last click position to detect infinite loops
                let mut last_click_pos: Option<(u32, u32)> = None;
                let mut duplicate_click_count = 0u32;
                const DUPLICATE_THRESHOLD: u32 = 3;
                const DUPLICATE_TOLERANCE: f64 = 20.0;
                // Track repeated scrolls at the same position to detect bottom-of-page loops
                let mut last_scroll: Option<(String, u32, u32)> = None;
                let mut duplicate_scroll_count = 0u32;
                const DUPLICATE_SCROLL_THRESHOLD: u32 = 4;

                // macOS Retina screens: model accuracy is much better when the
                // screenshot is sent at logical (point) size rather than physical
                // pixels. UI-TARS-desktop does the same resize before sending.
                let scale = if is_macos { display_logical_scale() } else { 1.0 };

                // Activate target app before first screenshot so UI-TARS operates
                // on the correct window, not whatever happens to be in front.
                if is_macos {
                    if let Some(app) = Self::extract_app_name(instruction) {
                        let _ = tokio::process::Command::new("osascript")
                            .args(["-e", &format!("tell application \"{}\" to activate", app)])
                            .output()
                            .await;
                        // Wait for the window to actually come to the front
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    }
                }

                for step in 0..max_steps {
                    // 1. Take screenshot at high quality for UI-TARS
                    let shot_result = self.tool_screenshot(None, Some(4096), Some(0)).await?;
                    let image_path = shot_result["image_path"]
                        .as_str()
                        .ok_or_else(|| anyhow!("ui_tars: screenshot returned no image_path"))?;

                    // 2. Resize to logical size for better model accuracy (macOS)
                    let (final_path, img_w, img_h, shot_w, shot_h) = if is_macos && scale > 1.0 {
                        let orig_w = shot_result["original_width"].as_u64().unwrap_or(2880) as u32;
                        let orig_h = shot_result["original_height"].as_u64().unwrap_or(1800) as u32;
                        let logical_w = (orig_w as f64 / scale).round() as u32;
                        let logical_h = (orig_h as f64 / scale).round() as u32;
                        let logical_path = std::env::temp_dir().join(format!("rsclaw_screen_logical_{}.png", step));
                        let logical_str = logical_path.to_string_lossy().to_string();
                        let resized = tokio::process::Command::new("sips")
                            .args([
                                "-z", &logical_h.to_string(), &logical_w.to_string(),
                                image_path, "--out", &logical_str,
                            ])
                            .output()
                            .await
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if resized {
                            if let Ok(bytes) = tokio::fs::read(&logical_path).await {
                                if let Some((w, h)) = crate::provider::ui_tars::image_dimensions(&bytes) {
                                    (logical_str, w, h, orig_w, orig_h)
                                } else {
                                    (logical_str, logical_w, logical_h, orig_w, orig_h)
                                }
                            } else {
                                (image_path.to_string(), orig_w, orig_h, orig_w, orig_h)
                            }
                        } else {
                            (image_path.to_string(), orig_w, orig_h, orig_w, orig_h)
                        }
                    } else {
                        let w = shot_result["width"].as_u64().unwrap_or(1920) as u32;
                        let h = shot_result["height"].as_u64().unwrap_or(1080) as u32;
                        let ow = shot_result["original_width"].as_u64().unwrap_or(w as u64) as u32;
                        let oh = shot_result["original_height"].as_u64().unwrap_or(h as u64) as u32;
                        (image_path.to_string(), w, h, ow, oh)
                    };

                    // 3. Read and base64-encode screenshot
                    let image_bytes = tokio::fs::read(&final_path)
                        .await
                        .map_err(|e| anyhow!("ui_tars: failed to read screenshot: {e}"))?;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&image_bytes);

                    // 4. Predict next action from UI-TARS model
                    let actions = provider
                        .predict(
                            UI_TARS_SYSTEM_PROMPT,
                            instruction,
                            &b64,
                            img_w,
                            img_h,
                            shot_w,
                            shot_h,
                            &history,
                        )
                        .await
                        .map_err(|e| anyhow!("ui_tars: prediction failed at step {step}: {e}"))?;

                    if actions.is_empty() {
                        steps.push(json!({
                            "step": step,
                            "screenshot": image_path,
                            "thought": "No action predicted",
                            "action": null,
                            "result": "empty prediction"
                        }));
                        break;
                    }

                    let action = &actions[0];
                    let action_type = action.action_type.as_str();

                    // 4. Terminal actions — finish or ask user
                    if action_type == "finished" || action_type == "finish" {
                        let content = action
                            .action_inputs
                            .get("content")
                            .cloned()
                            .unwrap_or_else(|| "task completed".to_string());
                        steps.push(json!({
                            "step": step,
                            "screenshot": image_path,
                            "thought": action.thought,
                            "action": action_type,
                            "result": content
                        }));
                        break;
                    }
                    if action_type == "call_user" {
                        let content = action
                            .action_inputs
                            .get("content")
                            .cloned()
                            .unwrap_or_default();
                        steps.push(json!({
                            "step": step,
                            "screenshot": image_path,
                            "thought": action.thought,
                            "action": action_type,
                            "result": content
                        }));
                        break;
                    }

                    // 5. Build execution args from predicted action.
                    // The model sees a logical-sized image, so its coordinates are in
                    // logical (point) space.  tool_mouse_click divides by scale before
                    // passing to cliclick, so we multiply here to end up with the
                    // correct logical coordinates on screen.
                    let exec_args = match action_type {
                        "click" | "left_click" => {
                            let x = action
                                .action_inputs
                                .get("start_box_x")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            let y = action
                                .action_inputs
                                .get("start_box_y")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            json!({"action": "mouse_click", "x": x, "y": y})
                        }
                        "left_double" | "double_click" => {
                            let x = action
                                .action_inputs
                                .get("start_box_x")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            let y = action
                                .action_inputs
                                .get("start_box_y")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            json!({"action": "double_click", "x": x, "y": y})
                        }
                        "right_single" | "right_click" => {
                            let x = action
                                .action_inputs
                                .get("start_box_x")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            let y = action
                                .action_inputs
                                .get("start_box_y")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            json!({"action": "right_click", "x": x, "y": y})
                        }
                        "drag" => {
                            let x1 = action
                                .action_inputs
                                .get("start_box_x")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            let y1 = action
                                .action_inputs
                                .get("start_box_y")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            let x2 = action
                                .action_inputs
                                .get("end_box_x")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            let y2 = action
                                .action_inputs
                                .get("end_box_y")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            json!({"action": "drag", "x": x1, "y": y1, "to_x": x2, "to_y": y2})
                        }
                        "hotkey" => {
                            let key = action
                                .action_inputs
                                .get("key")
                                .cloned()
                                .unwrap_or_default();
                            // UI-TARS outputs key=['ctrl','c'] format; normalize to ctrl+c
                            let key_normalized = key
                                .trim_start_matches("[")
                                .trim_end_matches("]")
                                .replace("', '", "+")
                                .replace("','", "+")
                                .replace("'", "");
                            json!({"action": "key", "key": key_normalized})
                        }
                        "type" | "type_text" => {
                            let content = action
                                .action_inputs
                                .get("content")
                                .cloned()
                                .unwrap_or_default();
                            // UI-TARS uses "\n" at end of content to submit.
                            if content.ends_with('\n') {
                                let text = content.trim_end_matches('\n').to_string();
                                // We can't easily chain type+key in one computer_use call.
                                // For now just type the text; the loop will see the
                                // textarea still has focus and model can send Enter next.
                                json!({"action": "type", "text": text})
                            } else {
                                json!({"action": "type", "text": content})
                            }
                        }
                        "scroll" => {
                            let x = action
                                .action_inputs
                                .get("start_box_x")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            let y = action
                                .action_inputs
                                .get("start_box_y")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0)
                                * scale;
                            let direction = action
                                .action_inputs
                                .get("direction")
                                .cloned()
                                .unwrap_or_else(|| "down".to_string());
                            json!({
                                "action": "scroll",
                                "x": x,
                                "y": y,
                                "direction": direction,
                                "amount": 3
                            })
                        }
                        "wait" => {
                            let ms = action
                                .action_inputs
                                .get("ms")
                                .and_then(|v| v.parse::<u64>().ok())
                                .unwrap_or(500);
                            json!({"action": "wait", "ms": ms.min(5000)})
                        }
                        other => {
                            steps.push(json!({
                                "step": step,
                                "screenshot": image_path,
                                "thought": action.thought,
                                "action": action_type,
                                "result": format!("unknown action type: {other}")
                            }));
                            break;
                        }
                    };

                    // Detect duplicate clicks (possible infinite loop)
                    if let (Some(x), Some(y)) = (exec_args.get("x"), exec_args.get("y")) {
                        if let (Some(xf), Some(yf)) = (x.as_f64(), y.as_f64()) {
                            let cx = xf as u32;
                            let cy = yf as u32;
                            if let Some((lx, ly)) = last_click_pos {
                                let dx = (cx as f64 - lx as f64).abs();
                                let dy = (cy as f64 - ly as f64).abs();
                                if dx < DUPLICATE_TOLERANCE && dy < DUPLICATE_TOLERANCE {
                                    duplicate_click_count += 1;
                                    if duplicate_click_count >= DUPLICATE_THRESHOLD {
                                        steps.push(json!({
                                            "step": step,
                                            "screenshot": image_path,
                                            "thought": action.thought,
                                            "action": action_type,
                                            "result": format!(
                                                "duplicate click detected at ({cx},{cy}) — possible loop, aborting"
                                            )
                                        }));
                                        break;
                                    }
                                } else {
                                    duplicate_click_count = 0;
                                }
                            }
                            last_click_pos = Some((cx, cy));
                        }
                    }

                    // Detect repeated scrolls at the same spot (bottom-of-page loop)
                    if action_type == "scroll" {
                        let dir = exec_args.get("direction").and_then(|v| v.as_str()).unwrap_or("down");
                        if let (Some(x), Some(y)) = (exec_args.get("x"), exec_args.get("y")) {
                            if let (Some(xf), Some(yf)) = (x.as_f64(), y.as_f64()) {
                                let sx = xf as u32;
                                let sy = yf as u32;
                                if let Some((last_dir, lx, ly)) = last_scroll {
                                    if last_dir == dir {
                                        let dx = (sx as f64 - lx as f64).abs();
                                        let dy = (sy as f64 - ly as f64).abs();
                                        if dx < DUPLICATE_TOLERANCE && dy < DUPLICATE_TOLERANCE {
                                            duplicate_scroll_count += 1;
                                            if duplicate_scroll_count >= DUPLICATE_SCROLL_THRESHOLD {
                                                steps.push(json!({
                                                    "step": step,
                                                    "screenshot": image_path,
                                                    "thought": action.thought,
                                                    "action": action_type,
                                                    "result": format!(
                                                        "duplicate scroll detected at ({sx},{sy}) direction={dir} — likely at bottom of page, aborting"
                                                    )
                                                }));
                                                break;
                                            }
                                        } else {
                                            duplicate_scroll_count = 0;
                                        }
                                    } else {
                                        duplicate_scroll_count = 0;
                                    }
                                }
                                last_scroll = Some((dir.to_string(), sx, sy));
                            }
                        }
                    }

                    // 6. Execute the action (Box::pin breaks async recursion)
                    let exec_result = Box::pin(self.tool_computer_use(exec_args)).await;
                    let result = match &exec_result {
                        Ok(v) => v.clone(),
                        Err(e) => json!({"error": e.to_string()}),
                    };

                    steps.push(json!({
                        "step": step,
                        "screenshot": image_path,
                        "thought": action.thought,
                        "action": action_type,
                        "result": result
                    }));

                    // 7. Add to history for next prediction
                    let action_summary = format!(
                        "{}({})",
                        action_type,
                        action
                            .action_inputs
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    history.push((action.thought.clone(), action_summary));

                    // Cap history to avoid bloating the context (keep last 10 steps)
                    if history.len() > 10 {
                        history.remove(0);
                    }

                    // 8. Brief pause after non-wait actions so the UI can update
                    if action_type != "wait" {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    }
                }

                let completed = steps
                    .last()
                    .and_then(|s| s["action"].as_str())
                    .map(|a| a == "finished" || a == "finish" || a == "call_user")
                    .unwrap_or(false);

                Ok(json!({
                    "action": "ui_tars",
                    "instruction": instruction,
                    "steps_taken": steps.len(),
                    "completed": completed,
                    "steps": steps
                }))
            }

            other => Err(anyhow!(
                "computer_use: unsupported action `{other}` \
                 (supported: screenshot, mouse_move, mouse_click, double_click, triple_click, \
                 right_click, middle_click, drag, scroll, type, key, hold_key, cursor_position, \
                 get_active_window, ui_tree, list_app_rules, get_app_rule, wait, ui_tars)"
            )),
        }
    }

    /// Analyze a screenshot with the local UI-TARS VLM and return detected UI elements.
    pub(crate) async fn tool_ui_analyze(&self, args: Value) -> Result<Value> {
        let image_path = args["image_path"]
            .as_str()
            .ok_or_else(|| anyhow!("ui_analyze: `image_path` required"))?;
        let max_tokens = args["max_tokens"].as_u64().unwrap_or(400) as u32;

        // Read config for API URL, key, and model.
        let (api_url, api_key, model) = self
            .config
            .raw
            .tools
            .as_ref()
            .and_then(|t| t.computer_use.as_ref())
            .map(|cu| {
                (
                    cu.ui_analyze_api_url.clone(),
                    cu.ui_analyze_api_key.clone(),
                    cu.ui_analyze_model.clone(),
                )
            })
            .unwrap_or((None, None, None));

        let Some(api_url) = api_url else {
            return Ok(json!({
                "action": "ui_analyze",
                "image": image_path,
                "count": 0,
                "elements": [],
                "note": "ui_analyze is not configured (tools.computerUse.uiAnalyzeApiUrl). Use computer_use ui_tree or screenshot reasoning instead.",
            }));
        };

        let mut provider = crate::provider::ui_tars::UiTarsProvider::new(api_url, api_key);
        if let Some(model) = model {
            provider = provider.with_model(model);
        }

        let elements = provider.analyze(image_path, max_tokens).await?;

        // Use the image file's actual dimensions as the coordinate reference.
        // detect_screen_size() may return a different display's resolution,
        // causing coordinate misalignment.
        let (img_w, img_h) = image::image_dimensions(image_path)
            .unwrap_or((1920, 1080));
        let scaled = crate::provider::ui_tars::UiTarsProvider::scale_coords(&elements, img_w, img_h);

        Ok(json!({
            "action": "ui_analyze",
            "image": image_path,
            "count": scaled.len(),
            "elements": scaled,
        }))
    }
}

/// Detect the primary screen resolution (physical pixels).
///
/// NOTE: Prefer using the screenshot's original_width/original_height for
/// coordinate mapping, since detect_screen_size may return a different
/// display's resolution in multi-monitor setups.
#[allow(dead_code)]
///
/// Cross-platform: macOS (AppleScript), Windows (PowerShell), Linux (xdotool).
/// Falls back to (1920, 1080) if detection fails.
async fn detect_screen_size() -> Option<(u32, u32)> {
    #[cfg(target_os = "macos")]
    {
        // Use CoreGraphics to get physical pixel dimensions, not logical points.
        // `screencapture` outputs physical pixels, so all coordinate math must
        // be in the same space.
        let output = tokio::process::Command::new("swift")
            .args([
                "-e",
                "import CoreGraphics; let id = CGMainDisplayID(); \"\\(CGDisplayPixelsWide(id)) \\(CGDisplayPixelsHigh(id))\"",
            ])
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut nums = text.split_whitespace();
        let w = nums.next()?.parse::<u32>().ok()?;
        let h = nums.next()?.parse::<u32>().ok()?;
        if w > 0 && h > 0 {
            return Some((w, h));
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Use PowerShell to query the primary screen physical resolution.
        // PrimaryScreen.Bounds returns the screen bounds in physical pixels.
        let output = powershell_hidden()
            .args([
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; \
                 $s = [System.Windows.Forms.Screen]::PrimaryScreen; \
                 \"$($s.Bounds.Width) $($s.Bounds.Height)\"",
            ])
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut nums = text.split_whitespace();
        let w = nums.next()?.parse::<u32>().ok()?;
        let h = nums.next()?.parse::<u32>().ok()?;
        return Some((w, h));
    }

    #[cfg(target_os = "linux")]
    {
        let output = tokio::process::Command::new("xdotool")
            .arg("getdisplaygeometry")
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut nums = text.split_whitespace();
        let w = nums.next()?.parse::<u32>().ok()?;
        let h = nums.next()?.parse::<u32>().ok()?;
        return Some((w, h));
    }

    None
}

/// Normalize UI-TARS / LLM key names to cliclick-compatible special-key names.
///
/// UI-TARS often outputs snake_case or camelCase variants (e.g. "pagedown",
/// "page_down", "PageDown") that cliclick does not recognise, causing the key
/// handler to fall through to `keystroke` and literally type the word into the
/// focused text field.  This maps common variants to the hyphenated names
/// cliclick expects.
fn normalize_key_name(key: &str) -> String {
    match key.to_lowercase().as_str() {
        "pagedown" | "page_down" => "page-down".to_owned(),
        "pageup" | "page_up" => "page-up".to_owned(),
        "arrowdown" | "arrow_down" | "down" => "arrow-down".to_owned(),
        "arrowup" | "arrow_up" | "up" => "arrow-up".to_owned(),
        "arrowleft" | "arrow_left" | "left" => "arrow-left".to_owned(),
        "arrowright" | "arrow_right" | "right" => "arrow-right".to_owned(),
        _ => key.to_lowercase(),
    }
}
