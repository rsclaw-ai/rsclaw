//! Self-owned, **script-based** cross-platform screen capture + window
//! enumeration — the in-tree replacement for the `xcap` crate.
//!
//! Why not xcap: on macOS xcap routes through ScreenCaptureKit, which on every
//! single grab re-enumerates `SCShareableContent` (a window-server round trip)
//! and spins up a fresh capture session — ~1.5s per call, fatal for a tight
//! monitor loop. It also breaks the Linux build (pipewire/libspa chain), so
//! capture was simply disabled there. Shelling out to the OS-native capture
//! tools is both far faster (Apple's `screencapture` is ~0.1s) and makes Linux
//! work again.
//!
//! Backends:
//! - **macOS**: `screencapture` for pixels, `python3`+Quartz for window list /
//!   scale factor (python is already required on macOS for the Vision OCR path).
//! - **Windows**: PowerShell + System.Drawing (capture) and user32 P/Invoke
//!   (window list / per-window capture via PrintWindow).
//! - **Linux**: `grim` (Wayland) → `scrot` → ImageMagick `import` (X11) for
//!   pixels; `wmctrl` for the window list.
//!
//! All coordinates are in **logical** screen points. Capture outputs are native
//! resolution (e.g. 2× on Retina); use [`primary_scale_factor`] when a caller
//! needs to map back to logical pixels.

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// An on-screen window. `id` is the OS-native window identifier (CGWindowID on
/// macOS, HWND on Windows, X11 window id on Linux). Bounds are logical points.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub app: String,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub minimized: bool,
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_png(tag: &str) -> PathBuf {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rsclaw_cap_{}_{tag}_{n}.png", std::process::id()))
}

/// Decode PNG bytes into an RGBA image (for pixel scanning).
pub fn png_to_rgba(bytes: &[u8]) -> Result<image::RgbaImage> {
    let img = image::load_from_memory(bytes).map_err(|e| anyhow!("decode PNG: {e}"))?;
    Ok(img.to_rgba8())
}

/// Wrap PNG bytes as a `data:image/png;base64,...` URI.
pub fn png_to_data_uri(bytes: &[u8]) -> String {
    use base64::Engine;
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

// ===========================================================================
// macOS
// ===========================================================================
#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    fn run_screencapture(args: &[String], out: &PathBuf) -> Result<Vec<u8>> {
        let status = Command::new("/usr/sbin/screencapture")
            .args(args)
            .arg(out)
            .output()
            .map_err(|e| anyhow!("screencapture spawn failed: {e}"))?;
        if !status.status.success() {
            let _ = std::fs::remove_file(out);
            return Err(anyhow!(
                "screencapture exited {}: {}",
                status.status,
                String::from_utf8_lossy(&status.stderr).trim()
            ));
        }
        let bytes = std::fs::read(out).map_err(|e| anyhow!("read capture: {e}"))?;
        let _ = std::fs::remove_file(out);
        Ok(bytes)
    }

    pub fn capture_region_png(x: i32, y: i32, w: u32, h: u32) -> Result<Vec<u8>> {
        let out = tmp_png("region");
        run_screencapture(
            &["-x".into(), format!("-R{x},{y},{w},{h}")],
            &out,
        )
    }

    pub fn capture_full_png() -> Result<Vec<u8>> {
        let out = tmp_png("full");
        run_screencapture(&["-x".into()], &out)
    }

    pub fn capture_window_png(window_id: u64) -> Result<Vec<u8>> {
        let out = tmp_png("win");
        // -o: no window shadow; -l<id>: capture that exact window (occlusion-proof).
        run_screencapture(&["-x".into(), "-o".into(), format!("-l{window_id}")], &out)
    }

    const WINLIST_PY: &str = r#"import json, Quartz
wl = Quartz.CGWindowListCopyWindowInfo(
    Quartz.kCGWindowListOptionOnScreenOnly | Quartz.kCGWindowListExcludeDesktopElements,
    Quartz.kCGNullWindowID)
out = []
for w in wl or []:
    b = w.get('kCGWindowBounds', {})
    out.append({
        'id': int(w.get('kCGWindowNumber', 0)),
        'app': w.get('kCGWindowOwnerName', '') or '',
        'title': w.get('kCGWindowName', '') or '',
        'x': int(b.get('X', 0)), 'y': int(b.get('Y', 0)),
        'w': int(b.get('Width', 0)), 'h': int(b.get('Height', 0)),
        'minimized': not bool(w.get('kCGWindowIsOnscreen', True)),
    })
print(json.dumps(out))
"#;

    pub fn list_windows() -> Result<Vec<WindowInfo>> {
        let out = Command::new("python3")
            .args(["-c", WINLIST_PY])
            .output()
            .map_err(|e| anyhow!("python3 window list spawn failed (need pyobjc Quartz): {e}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "window list failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let wins: Vec<WindowInfo> = serde_json::from_slice(&out.stdout)
            .map_err(|e| anyhow!("parse window list: {e}"))?;
        Ok(wins)
    }

    const SCALE_PY: &str = r#"import Quartz
d = Quartz.CGMainDisplayID()
px = Quartz.CGDisplayPixelsWide(d)
b = Quartz.CGDisplayBounds(d)
pts = b.size.width
print(px / pts if pts else 1.0)
"#;

    pub fn primary_scale_factor() -> f32 {
        Command::new("python3")
            .args(["-c", SCALE_PY])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f32>().ok())
            .filter(|s| *s > 0.0)
            .unwrap_or(2.0)
    }
}

// ===========================================================================
// Windows
// ===========================================================================
#[cfg(target_os = "windows")]
mod imp {
    use super::*;

    fn run_powershell(script: &str, out: &PathBuf) -> Result<Vec<u8>> {
        let status = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
            .map_err(|e| anyhow!("powershell spawn failed: {e}"))?;
        if !status.status.success() {
            let _ = std::fs::remove_file(out);
            return Err(anyhow!(
                "powershell capture failed: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            ));
        }
        let bytes = std::fs::read(out).map_err(|e| anyhow!("read capture: {e}"))?;
        let _ = std::fs::remove_file(out);
        Ok(bytes)
    }

    pub fn capture_region_png(x: i32, y: i32, w: u32, h: u32) -> Result<Vec<u8>> {
        let out = tmp_png("region");
        let p = out.display();
        let script = format!(
            "Add-Type -AssemblyName System.Drawing; \
             $bmp = New-Object System.Drawing.Bitmap({w}, {h}); \
             $g = [System.Drawing.Graphics]::FromImage($bmp); \
             $g.CopyFromScreen({x}, {y}, 0, 0, $bmp.Size); \
             $bmp.Save('{p}', [System.Drawing.Imaging.ImageFormat]::Png); \
             $g.Dispose(); $bmp.Dispose()"
        );
        run_powershell(&script, &out)
    }

    pub fn capture_full_png() -> Result<Vec<u8>> {
        let out = tmp_png("full");
        let p = out.display();
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             Add-Type -AssemblyName System.Drawing; \
             $b = [System.Windows.Forms.SystemInformation]::VirtualScreen; \
             $bmp = New-Object System.Drawing.Bitmap($b.Width, $b.Height); \
             $g = [System.Drawing.Graphics]::FromImage($bmp); \
             $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size); \
             $bmp.Save('{p}', [System.Drawing.Imaging.ImageFormat]::Png); \
             $g.Dispose(); $bmp.Dispose()"
        );
        run_powershell(&script, &out)
    }

    pub fn capture_window_png(window_id: u64) -> Result<Vec<u8>> {
        // PrintWindow the HWND (captures even if partially occluded).
        let out = tmp_png("win");
        let p = out.display();
        let script = format!(
            r#"Add-Type -AssemblyName System.Drawing;
$sig = @'
using System;
using System.Runtime.InteropServices;
public class W {{
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
  [StructLayout(LayoutKind.Sequential)] public struct RECT {{ public int L,T,R,B; }}
}}
'@;
Add-Type $sig;
$h = [IntPtr]{window_id};
$r = New-Object W+RECT;
[void][W]::GetWindowRect($h, [ref]$r);
$w = $r.R - $r.L; $ht = $r.B - $r.T;
$bmp = New-Object System.Drawing.Bitmap($w, $ht);
$g = [System.Drawing.Graphics]::FromImage($bmp);
$dc = $g.GetHdc();
[void][W]::PrintWindow($h, $dc, 2);
$g.ReleaseHdc($dc);
$bmp.Save('{p}', [System.Drawing.Imaging.ImageFormat]::Png);
$g.Dispose(); $bmp.Dispose()"#
        );
        run_powershell(&script, &out)
    }

    pub fn list_windows() -> Result<Vec<WindowInfo>> {
        let script = r#"Add-Type -AssemblyName System.Text.Json -ErrorAction SilentlyContinue
$sig = @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WL {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
  public static List<string> Run() {
    var res = new List<string>();
    EnumWindows((h,p) => {
      if (!IsWindowVisible(h)) return true;
      var sb = new StringBuilder(512); GetWindowText(h, sb, 512);
      RECT r; GetWindowRect(h, out r);
      uint pid; GetWindowThreadProcessId(h, out pid);
      string app=""; try { app = System.Diagnostics.Process.GetProcessById((int)pid).ProcessName; } catch {}
      int w=r.R-r.L, ht=r.B-r.T;
      if (w<=0||ht<=0) return true;
      res.Add(((long)h)+"\t"+app+"\t"+sb.ToString().Replace("\t"," ")+"\t"+r.L+"\t"+r.T+"\t"+w+"\t"+ht);
      return true;
    }, IntPtr.Zero);
    return res;
  }
}
'@
Add-Type $sig
[WL]::Run() | ForEach-Object { $_ }"#;
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
            .map_err(|e| anyhow!("powershell window list spawn failed: {e}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "window list failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut wins = Vec::new();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 7 {
                continue;
            }
            wins.push(WindowInfo {
                id: f[0].trim().parse().unwrap_or(0),
                app: f[1].to_string(),
                title: f[2].to_string(),
                x: f[3].trim().parse().unwrap_or(0),
                y: f[4].trim().parse().unwrap_or(0),
                w: f[5].trim().parse().unwrap_or(0),
                h: f[6].trim().parse().unwrap_or(0),
                minimized: false,
            });
        }
        Ok(wins)
    }

    pub fn primary_scale_factor() -> f32 {
        1.0
    }
}

// ===========================================================================
// Linux
// ===========================================================================
#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    fn have(cmd: &str) -> bool {
        which::which(cmd).is_ok()
    }

    fn read_consume(out: &PathBuf) -> Result<Vec<u8>> {
        let bytes = std::fs::read(out).map_err(|e| anyhow!("read capture: {e}"))?;
        let _ = std::fs::remove_file(out);
        Ok(bytes)
    }

    pub fn capture_region_png(x: i32, y: i32, w: u32, h: u32) -> Result<Vec<u8>> {
        let out = tmp_png("region");
        // Wayland: grim -g "X,Y WxH". X11: ImageMagick import / scrot -a.
        if have("grim") {
            let st = Command::new("grim")
                .args(["-g", &format!("{x},{y} {w}x{h}")])
                .arg(&out)
                .status();
            if matches!(st, Ok(s) if s.success()) {
                return read_consume(&out);
            }
        }
        if have("import") {
            let st = Command::new("import")
                .args(["-window", "root", "-crop", &format!("{w}x{h}+{x}+{y}")])
                .arg(&out)
                .status();
            if matches!(st, Ok(s) if s.success()) {
                return read_consume(&out);
            }
        }
        if have("scrot") {
            let st = Command::new("scrot")
                .args(["-a", &format!("{x},{y},{w},{h}")])
                .arg(&out)
                .status();
            if matches!(st, Ok(s) if s.success()) {
                return read_consume(&out);
            }
        }
        Err(anyhow!(
            "no Linux screenshot tool found (install one of: grim, imagemagick, scrot)"
        ))
    }

    pub fn capture_full_png() -> Result<Vec<u8>> {
        let out = tmp_png("full");
        if have("grim") {
            let st = Command::new("grim").arg(&out).status();
            if matches!(st, Ok(s) if s.success()) {
                return read_consume(&out);
            }
        }
        if have("scrot") {
            let st = Command::new("scrot").arg(&out).status();
            if matches!(st, Ok(s) if s.success()) {
                return read_consume(&out);
            }
        }
        if have("import") {
            let st = Command::new("import").args(["-window", "root"]).arg(&out).status();
            if matches!(st, Ok(s) if s.success()) {
                return read_consume(&out);
            }
        }
        Err(anyhow!(
            "no Linux screenshot tool found (install one of: grim, scrot, imagemagick)"
        ))
    }

    pub fn capture_window_png(window_id: u64) -> Result<Vec<u8>> {
        let out = tmp_png("win");
        // X11 only: capture the specific window by id via ImageMagick import.
        if have("import") {
            let st = Command::new("import")
                .args(["-window", &window_id.to_string()])
                .arg(&out)
                .status();
            if matches!(st, Ok(s) if s.success()) {
                return read_consume(&out);
            }
        }
        // Fallback: capture the window's region from its bounds.
        if let Ok(wins) = list_windows() {
            if let Some(w) = wins.iter().find(|w| w.id == window_id) {
                return capture_region_png(w.x, w.y, w.w, w.h);
            }
        }
        Err(anyhow!(
            "window capture needs ImageMagick `import` (X11) or a resolvable window region"
        ))
    }

    pub fn list_windows() -> Result<Vec<WindowInfo>> {
        if !have("wmctrl") {
            return Err(anyhow!("window list needs `wmctrl` on Linux"));
        }
        // `wmctrl -lG`: id desktop x y w h host title ; `-lx` adds WM_CLASS for app.
        let geo = Command::new("wmctrl")
            .args(["-lG"])
            .output()
            .map_err(|e| anyhow!("wmctrl spawn failed: {e}"))?;
        if !geo.status.success() {
            return Err(anyhow!(
                "wmctrl failed: {}",
                String::from_utf8_lossy(&geo.stderr).trim()
            ));
        }
        let class = Command::new("wmctrl").args(["-lx"]).output().ok();
        let class_txt = class
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let mut app_by_id = std::collections::HashMap::new();
        for line in class_txt.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 3 {
                if let Ok(id) = u64::from_str_radix(f[0].trim_start_matches("0x"), 16) {
                    app_by_id.insert(id, f[2].to_string());
                }
            }
        }
        let text = String::from_utf8_lossy(&geo.stdout);
        let mut wins = Vec::new();
        for line in text.lines() {
            let f: Vec<&str> = line.splitn(8, char::is_whitespace).collect();
            if f.len() < 8 {
                continue;
            }
            let id = u64::from_str_radix(f[0].trim_start_matches("0x"), 16).unwrap_or(0);
            wins.push(WindowInfo {
                id,
                app: app_by_id.get(&id).cloned().unwrap_or_default(),
                title: f[7].to_string(),
                x: f[2].parse().unwrap_or(0),
                y: f[3].parse().unwrap_or(0),
                w: f[4].parse().unwrap_or(0),
                h: f[5].parse().unwrap_or(0),
                minimized: false,
            });
        }
        Ok(wins)
    }

    pub fn primary_scale_factor() -> f32 {
        1.0
    }
}

// ===========================================================================
// Public API (delegates to the platform module)
// ===========================================================================

/// Capture a region (logical points) of the screen → PNG bytes.
pub fn capture_region_png(x: i32, y: i32, w: u32, h: u32) -> Result<Vec<u8>> {
    imp::capture_region_png(x, y, w, h)
}

/// Capture the full (primary/virtual) screen → PNG bytes.
pub fn capture_full_png() -> Result<Vec<u8>> {
    imp::capture_full_png()
}

/// Capture a specific window by its OS-native id → PNG bytes.
pub fn capture_window_png(window_id: u64) -> Result<Vec<u8>> {
    imp::capture_window_png(window_id)
}

/// Enumerate on-screen windows (front-to-back where the OS provides ordering).
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    imp::list_windows()
}

/// Primary-display scale factor (2.0 on macOS Retina, 1.0 elsewhere by default).
pub fn primary_scale_factor() -> f32 {
    imp::primary_scale_factor()
}

/// Convenience: capture a region directly as an RGBA image.
pub fn capture_region_rgba(x: i32, y: i32, w: u32, h: u32) -> Result<image::RgbaImage> {
    png_to_rgba(&capture_region_png(x, y, w, h)?)
}
