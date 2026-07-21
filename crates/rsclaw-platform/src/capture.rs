//! Self-owned cross-platform screen capture + window enumeration — the in-tree
//! replacement for the `xcap` crate.
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
//! - **macOS**: `screencapture` for pixels, CoreGraphics FFI for window list /
//!   scale factor.
//! - **Windows**: PowerShell + System.Drawing (capture) and user32 P/Invoke
//!   (window list / per-window capture via PrintWindow).
//! - **Linux**: `grim` (Wayland) → `scrot` → ImageMagick `import` (X11) for
//!   pixels; `wmctrl` for the window list.
//!
//! All coordinates are in **logical** screen points. Capture outputs are native
//! resolution (e.g. 2× on Retina); use [`primary_scale_factor`] when a caller
//! needs to map back to logical pixels.

use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, anyhow};

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
        run_screencapture(&["-x".into(), format!("-R{x},{y},{w},{h}")], &out)
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

    // ---------------------------------------------------------------
    // CoreGraphics FFI
    // ---------------------------------------------------------------

    use core_foundation::{
        base::{CFType, TCFType},
        number::CFNumber,
        string::CFString,
    };

    /// `CGRect` as returned by `CGDisplayBounds` (all `f64` / `CGFloat`).
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    type CGWindowID = u32;
    type CGDirectDisplayID = u32;

    /// Option flags for `CGWindowListCopyWindowInfo`.
    const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
    const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
    const K_CG_NULL_WINDOW_ID: CGWindowID = 0;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(
            option: u32,
            relative_to_window: CGWindowID,
        ) -> core_foundation::array::CFArrayRef;
        fn CGMainDisplayID() -> CGDirectDisplayID;
        fn CGDisplayPixelsWide(display: CGDirectDisplayID) -> usize;
        fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDictionaryGetValue(
            dict: core_foundation::dictionary::CFDictionaryRef,
            key: *const std::ffi::c_void,
        ) -> *const std::ffi::c_void;
        fn CFArrayGetCount(arr: core_foundation::array::CFArrayRef) -> isize;
        fn CFArrayGetValueAtIndex(
            arr: core_foundation::array::CFArrayRef,
            idx: isize,
        ) -> *const std::ffi::c_void;
        fn CFRelease(cf: *const std::ffi::c_void);
    }

    /// Look up a key in a raw `CFDictionaryRef` and return the value as a
    /// `CFType` (GET rule — caller does not own the returned reference).
    /// Returns `None` if the key is absent.
    fn dict_lookup(
        dict: core_foundation::dictionary::CFDictionaryRef,
        key: &str,
    ) -> Option<CFType> {
        let cf_key = CFString::new(key);
        let val = unsafe { CFDictionaryGetValue(dict, cf_key.as_concrete_TypeRef() as *const _) };
        if val.is_null() {
            None
        } else {
            Some(unsafe { CFType::wrap_under_get_rule(val as core_foundation::base::CFTypeRef) })
        }
    }

    /// Helper: look up a key in a `CFDictionary` and downcast it to a
    /// `CFNumber`, returning its `i64` value.
    fn dict_get_number(
        dict: core_foundation::dictionary::CFDictionaryRef,
        key: &str,
    ) -> Option<i64> {
        dict_lookup(dict, key)?
            .downcast::<CFNumber>()
            .and_then(|n| n.to_i64())
    }

    /// Helper: look up a key in a `CFDictionary` and downcast it to a
    /// `CFString`, returning a Rust `String`.
    fn dict_get_string(
        dict: core_foundation::dictionary::CFDictionaryRef,
        key: &str,
    ) -> Option<String> {
        dict_lookup(dict, key)?
            .downcast::<CFString>()
            .map(|s| s.to_string())
    }

    /// Helper: look up a key in a `CFDictionary` and return the raw
    /// `CFDictionaryRef` for a nested dictionary.
    fn dict_get_dict_ref(
        dict: core_foundation::dictionary::CFDictionaryRef,
        key: &str,
    ) -> Option<core_foundation::dictionary::CFDictionaryRef> {
        let cf_key = CFString::new(key);
        let val = unsafe { CFDictionaryGetValue(dict, cf_key.as_concrete_TypeRef() as *const _) };
        if val.is_null() {
            None
        } else {
            Some(val as core_foundation::dictionary::CFDictionaryRef)
        }
    }

    /// Helper: look up a key in a `CFDictionary` and interpret it as a
    /// boolean (CFBoolean or CFNumber).
    fn dict_get_bool(
        dict: core_foundation::dictionary::CFDictionaryRef,
        key: &str,
    ) -> Option<bool> {
        let cf = dict_lookup(dict, key)?;
        // Try CFBoolean first (macOS actually stores kCGWindowIsOnscreen this way).
        if let Some(b) = cf.downcast::<core_foundation::boolean::CFBoolean>() {
            return Some(b == core_foundation::boolean::CFBoolean::true_value());
        }
        // Fall back to CFNumber.
        if let Some(n) = cf.downcast::<CFNumber>() {
            return Some(n.to_i64().unwrap_or(0) != 0);
        }
        None
    }

    pub fn list_windows() -> Result<Vec<WindowInfo>> {
        let cf_array_ref = unsafe {
            CGWindowListCopyWindowInfo(
                K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS,
                K_CG_NULL_WINDOW_ID,
            )
        };
        if cf_array_ref.is_null() {
            return Err(anyhow!("CGWindowListCopyWindowInfo returned NULL"));
        }
        let count = unsafe { CFArrayGetCount(cf_array_ref) };
        let mut wins = Vec::with_capacity(count as usize);
        for i in 0..count {
            let dict_ref = unsafe { CFArrayGetValueAtIndex(cf_array_ref, i) }
                as core_foundation::dictionary::CFDictionaryRef;
            if dict_ref.is_null() {
                continue;
            }

            let id = dict_get_number(dict_ref, "kCGWindowNumber").unwrap_or(0) as u64;
            let app = dict_get_string(dict_ref, "kCGWindowOwnerName").unwrap_or_default();
            let title = dict_get_string(dict_ref, "kCGWindowName").unwrap_or_default();

            let (x, y, w, h) =
                if let Some(bounds_ref) = dict_get_dict_ref(dict_ref, "kCGWindowBounds") {
                    (
                        dict_get_number(bounds_ref, "X").unwrap_or(0) as i32,
                        dict_get_number(bounds_ref, "Y").unwrap_or(0) as i32,
                        dict_get_number(bounds_ref, "Width").unwrap_or(0) as u32,
                        dict_get_number(bounds_ref, "Height").unwrap_or(0) as u32,
                    )
                } else {
                    (0, 0, 0, 0)
                };

            let is_onscreen = dict_get_bool(dict_ref, "kCGWindowIsOnscreen").unwrap_or(true);

            wins.push(WindowInfo {
                id,
                app,
                title,
                x,
                y,
                w,
                h,
                minimized: !is_onscreen,
            });
        }
        // Release the array (Create Rule — we own it).
        unsafe { CFRelease(cf_array_ref as *const std::ffi::c_void) };

        Ok(wins)
    }

    pub fn primary_scale_factor() -> f32 {
        let display = unsafe { CGMainDisplayID() };
        let pixels_wide = unsafe { CGDisplayPixelsWide(display) } as f64;
        let bounds = unsafe { CGDisplayBounds(display) };
        let logical_width = bounds.size.width;
        if logical_width > 0.0 {
            (pixels_wide / logical_width) as f32
        } else {
            2.0
        }
    }

    /// macOS apps don't destroy their window on close the way WeChat 4.x does
    /// on Windows; the activate path (`open -b`) already handles bringing
    /// them up.
    pub fn restore_app_window(_app_name: &str) -> Result<bool> {
        Ok(false)
    }
}

// ===========================================================================
// Windows
// ===========================================================================
#[cfg(target_os = "windows")]
mod imp {
    use super::*;

    fn run_powershell(script: &str, out: &PathBuf) -> Result<Vec<u8>> {
        use std::os::windows::process::CommandExt;
        let status = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            // CREATE_NO_WINDOW: no flashing console window per capture.
            .creation_flags(0x08000000)
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
        // DPI-aware so coords/sizes are PHYSICAL pixels (match enigo + full capture).
        let script = format!(
            "Add-Type -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool SetProcessDPIAware();' -Name DpiR -Namespace Win32 -ErrorAction SilentlyContinue; \
             try {{ [Win32.DpiR]::SetProcessDPIAware() | Out-Null }} catch {{}}; \
             Add-Type -AssemblyName System.Drawing; \
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
        // SetProcessDPIAware FIRST so on a scaled display (e.g. 2560x1440 @150%)
        // we capture PHYSICAL pixels (2560x1440), matching the coordinate space that
        // enigo SendInput uses for clicks. Without it CopyFromScreen captures the
        // logical 1707x960 surface and every OCR→click mapping is off by the DPI
        // scale (clicks land at ~1/1.5 of the intended position).
        let script = format!(
            "Add-Type -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool SetProcessDPIAware();' -Name DpiA -Namespace Win32 -ErrorAction SilentlyContinue; \
             try {{ [Win32.DpiA]::SetProcessDPIAware() | Out-Null }} catch {{}}; \
             Add-Type -AssemblyName System.Windows.Forms; \
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
        use std::os::windows::process::CommandExt;
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            // CREATE_NO_WINDOW: no flashing console window when listing windows.
            .creation_flags(0x08000000)
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

    /// Restore an app's hidden/tray main window. WeChat 4.x closes to the tray
    /// by destroying its top-level window, so we enumerate ALL top-level
    /// windows of the matching process (including invisible ones) whose
    /// title looks like the main window and SW_RESTORE + foreground them.
    /// Returns true if ≥1 restored.
    pub fn restore_app_window(app_name: &str) -> Result<bool> {
        use std::os::windows::process::CommandExt;
        let needle = app_name.to_lowercase();
        // Process-name aliases (WeChat 4.x renamed the process to "Weixin").
        let proc_match = if needle.contains("wechat") || needle.contains("weixin") {
            "weixin wechat".to_string()
        } else {
            needle.clone()
        };
        // Embed the process-name match list; titles are fixed to the known main
        // window captions so we don't surface IME/tray helper windows.
        let script = format!(
            r#"$procs = '{procs}'.Split(' ')
$sig = @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WR {{
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  public static int Restore(string[] procs, string[] titles) {{
    int n = 0;
    EnumWindows((h,p) => {{
      uint pid; GetWindowThreadProcessId(h, out pid);
      string pname = "";
      try {{ pname = System.Diagnostics.Process.GetProcessById((int)pid).ProcessName.ToLower(); }} catch {{ return true; }}
      bool pmatch = false; foreach (var pr in procs) {{ if (pr.Length > 0 && pname.Contains(pr)) pmatch = true; }}
      if (!pmatch) return true;
      var sb = new StringBuilder(512); GetWindowText(h, sb, 512);
      string t = sb.ToString();
      bool tmatch = false; foreach (var tt in titles) {{ if (t == tt) tmatch = true; }}
      if (!tmatch) return true;
      ShowWindow(h, 9); ShowWindow(h, 5); SetForegroundWindow(h); n++;
      return true;
    }}, IntPtr.Zero);
    return n;
  }}
}}
'@
Add-Type $sig
[WR]::Restore($procs, @('微信','Weixin','WeChat'))"#,
            procs = proc_match
        );
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            // CREATE_NO_WINDOW: no flashing console window.
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| anyhow!("powershell restore spawn failed: {e}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "restore window failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let n: i32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .lines()
            .last()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0);
        Ok(n > 0)
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
            let st = Command::new("import")
                .args(["-window", "root"])
                .arg(&out)
                .status();
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

    pub fn restore_app_window(_app_name: &str) -> Result<bool> {
        Ok(false)
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

/// Primary-display scale factor (2.0 on macOS Retina, 1.0 elsewhere by
/// default).
pub fn primary_scale_factor() -> f32 {
    imp::primary_scale_factor()
}

/// Restore an app's main window from the system tray / hidden state so it can
/// be captured. `app_name` is matched case-insensitively against the process
/// name (e.g. "WeChat"/"Weixin"). Returns `Ok(true)` if at least one window was
/// restored. No-op (returns `Ok(false)`) on platforms where it isn't needed.
///
/// Motivation: WeChat 4.x closes to the tray by destroying its top-level window
/// (MainWindowHandle becomes 0), so window enumeration finds nothing and OCR
/// has no surface. This brings the hidden main window back before capture.
pub fn restore_app_window(app_name: &str) -> Result<bool> {
    imp::restore_app_window(app_name)
}

/// Convenience: capture a region directly as an RGBA image.
pub fn capture_region_rgba(x: i32, y: i32, w: u32, h: u32) -> Result<image::RgbaImage> {
    png_to_rgba(&capture_region_png(x, y, w, h)?)
}
