//! Native desktop session backed by `enigo` (input) +
//! `rsclaw-platform::capture` (script-based screen capture + window
//! enumeration; replaces xcap).
//!
//! Input synthesis runs inside `tokio::task::spawn_blocking` with a fresh
//! `Enigo` per call because `Enigo` is not `Send` on Windows. Screenshots
//! are similarly blocking.

use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use enigo::{
    Button, Coordinate,
    Direction::{Click, Press, Release},
    Enigo, Key, Keyboard, Mouse, Settings,
};
use rsclaw_platform::capture;
use tracing::warn;

use super::DesktopSession;

/// Unit struct — holds no state because every enigo call needs a fresh
/// instance on the calling OS thread.
pub struct NativeDesktopSession;

struct PrivateTempFile {
    path: PathBuf,
}

impl PrivateTempFile {
    fn create(prefix: &str, suffix: &str) -> Result<Self, String> {
        static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);
        for _ in 0..32 {
            let nonce = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| format!("read system time for temp file: {e}"))?
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "{prefix}{}-{nanos}-{nonce}{suffix}",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("create private temp file: {e}")),
            }
        }
        Err("create private temp file: too many name collisions".to_owned())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateTempFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %self.path.display(), %error, "failed to remove private OCR temp file");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Construct a fresh `Enigo` or return an error string.
fn new_enigo() -> Result<Enigo, String> {
    Enigo::new(&Settings::default()).map_err(|e| {
        let is_perm =
            e.to_string().contains("permission") || e.to_string().contains("simulate input");
        let hint = if is_perm {
            #[cfg(target_os = "macos")]
            {
                // Surface the native macOS Accessibility dialog (once per process)
                // so the user actually gets asked — and the app lands in the
                // Accessibility list — instead of a silent permission failure with
                // no prompt.
                static PROMPTED: std::sync::Once = std::sync::Once::new();
                PROMPTED.call_once(|| {
                    let _ = crate::macos_perm::prompt_accessibility();
                });
                return format!(
                    "enigo init failed: {e} (macOS: a permission request was raised \
                    — enable RsClaw under System Settings → Privacy & Security → \
                    Accessibility AND Input Monitoring, then quit & reopen RsClaw)"
                );
            }
            #[cfg(not(target_os = "macos"))]
            " (check input permissions)"
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

/// Capture a region of the screen (logical points). Script-based capture grabs
/// exactly the requested rect, so no monitor enumeration / scale-crop needed.
fn capture_region(x: u32, y: u32, w: u32, h: u32) -> Result<String, String> {
    let png = capture::capture_region_png(x as i32, y as i32, w, h).map_err(|e| e.to_string())?;
    Ok(capture::png_to_data_uri(&png))
}

/// Count pixels in a screen region whose RGB is within `tol` (per channel) of
/// the target colour. Returns `(matching, total)`. The script capture returns
/// exactly the requested region (at native resolution), so we scan every pixel.
#[allow(clippy::too_many_arguments)]
fn region_color_count(
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    r: u32,
    g: u32,
    b: u32,
    tol: u32,
) -> Result<(u32, u32), String> {
    let img = capture::capture_region_rgba(x as i32, y as i32, w, h).map_err(|e| e.to_string())?;
    let (tr, tg, tb, tol) = (r as i32, g as i32, b as i32, tol as i32);
    let mut count: u32 = 0;
    let mut total: u32 = 0;
    for px in img.pixels() {
        let [pr, pg, pb, _pa] = px.0;
        total += 1;
        if (pr as i32 - tr).abs() <= tol
            && (pg as i32 - tg).abs() <= tol
            && (pb as i32 - tb).abs() <= tol
        {
            count += 1;
        }
    }
    Ok((count, total))
}

/// Capture the full primary monitor (fallback when window bounds unavailable).
fn capture_primary_monitor() -> Result<String, String> {
    let png = capture::capture_full_png().map_err(|e| e.to_string())?;
    Ok(capture::png_to_data_uri(&png))
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
/// Find the frontmost on-screen window of an app (≥200×200) via the
/// script-based window list. The list is front-to-back where the OS provides
/// ordering, so the first match is frontmost — letting one call serve both the
/// main window and a transient child (e.g. WeChat's merged-record viewer).
/// Virtual-screen rect (origin + size) via GetSystemMetrics — matches the area
/// `capture_full_png` grabs
/// (System.Windows.Forms.SystemInformation.VirtualScreen). Fast (no process
/// spawn); used so the plugin's full-screen 0-1000 coords convert to correct
/// screen pixels. SM_*VIRTUALSCREEN = 76..79.
#[cfg(target_os = "windows")]
fn virtual_screen_rect() -> (i32, i32, u32, u32) {
    unsafe extern "system" {
        fn GetDC(h: isize) -> isize;
        fn ReleaseDC(h: isize, dc: isize) -> i32;
        fn GetDeviceCaps(dc: isize, index: i32) -> i32;
    }
    // PHYSICAL screen size (DESKTOPHORZRES=118 / DESKTOPVERTRES=117) — independent
    // of this process's DPI awareness. capture_full_png also captures physical
    // pixels (SetProcessDPIAware), and enigo SendInput maps in physical space, so
    // returning physical here keeps OCR→0-1000→screen→click all in one coord space
    // on scaled displays (e.g. 2560x1440 @150%, where logical is 1707x960).
    unsafe {
        let dc = GetDC(0);
        let w = GetDeviceCaps(dc, 118).max(0) as u32;
        let h = GetDeviceCaps(dc, 117).max(0) as u32;
        ReleaseDC(0, dc);
        if w > 0 && h > 0 {
            (0, 0, w, h)
        } else {
            // Fallback to logical metrics if GetDeviceCaps fails.
            unsafe extern "system" {
                fn GetSystemMetrics(n: i32) -> i32;
            }
            (
                0,
                0,
                GetSystemMetrics(0).max(0) as u32,
                GetSystemMetrics(1).max(0) as u32,
            )
        }
    }
}

fn find_app_window(app_name: &str) -> Result<capture::WindowInfo, String> {
    let wins = capture::list_windows().map_err(|e| e.to_string())?;
    // App identity differs by platform: macOS list reports the app's display name
    // ("WeChat"/"Weixin"), Windows reports the process name ("Weixin"), and the
    // caller may pass a macOS bundle id ("com.tencent.xinWeChat"). Match the exact
    // name first, then known aliases (case-insensitive substring) so the same
    // plugin works on both OSes — esp. WeChat 4.x renamed the process to "Weixin".
    let target = app_name.to_lowercase();
    let aliases: &[&str] = if target.contains("wechat") || target.contains("weixin") {
        &["weixin", "wechat", "微信"]
    } else {
        &[]
    };
    let matches = |w: &capture::WindowInfo| {
        let a = w.app.to_lowercase();
        a == target
            || aliases
                .iter()
                .any(|al| a.contains(al) || w.app.contains(al))
    };
    // WeChat's process owns several non-UI helper top-level windows (a tray-message
    // sink, IME hosts, a splash). They can be larger than the real chat window but
    // render BLANK, so "pick the biggest weixin window" grabs the wrong one. Skip
    // them by their (ASCII, encoding-safe) titles; the real chat window is titled
    // "微信" and is the largest of what remains.
    const HELPER_TITLES: &[&str] = &[
        "WxTrayIconMessageWindow",
        "Default IME",
        "MSCTFIME UI",
        "Weixin", // English splash/loader window, not the chat UI
        "",       // untitled helper surfaces
    ];
    let qualifies = |w: &capture::WindowInfo| {
        matches(w)
            && !w.minimized
            && w.w >= 200
            && w.h >= 200
            && !HELPER_TITLES.contains(&w.title.as_str())
    };
    // Pick the LARGEST qualifying window (the chat UI), not just the first.
    let largest = |wins: &[capture::WindowInfo]| {
        wins.iter()
            .filter(|&w| qualifies(w))
            .max_by_key(|w| (w.w as u64) * (w.h as u64))
            .cloned()
    };
    if let Some(w) = largest(&wins) {
        return Ok(w);
    }
    // Nothing on-screen: the app may have closed to the tray (WeChat 4.x destroys
    // its top-level window on close). Try to restore the hidden main window, then
    // re-enumerate once before giving up — keeps the monitor loop self-healing.
    if capture::restore_app_window(app_name).unwrap_or(false) {
        std::thread::sleep(std::time::Duration::from_millis(800));
        let wins2 = capture::list_windows().map_err(|e| e.to_string())?;
        if let Some(w) = largest(&wins2) {
            return Ok(w);
        }
    }
    Err(format!("no on-screen window for app '{app_name}'"))
}

fn capture_app_window(app_name: &str) -> Result<String, String> {
    let win = find_app_window(app_name)?;
    // Capture the exact window by id — occlusion-proof (Apple `screencapture -l`
    // on macOS), handles hardware-accelerated / opaque windows like WeChat 4.x
    // that the old in-process xcap path hung on.
    let png = capture::capture_window_png(win.id).map_err(|e| e.to_string())?;
    if png.is_empty() {
        return Err("window capture produced an empty image (window off-screen?)".to_string());
    }
    Ok(capture::png_to_data_uri(&png))
}

/// On-device OCR of an app window. On Windows uses WinRT
/// `Windows.Media.Ocr`; on macOS native Vision FFI is planned (currently
/// returns an empty result and callers fall back to VLM grounding).
/// Returns a JSON array of recognised lines with 0-1000 relative centre
/// coords: `[{"text":"东升","x":143,"y":699}, ...]`.
/// Far more precise than VLM grounding for clicking a named row.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn ocr_window(_app_name: &str) -> Result<String, String> {
    Err("ocr_window only implemented on macOS and Windows".to_string())
}

/// Maximise a window (by HWND) and force it to the foreground, even over
/// another app (e.g. a stray File Explorer) — a background gateway process
/// can't normally win SetForegroundWindow, so we AttachThreadInput to the
/// current foreground thread first to bypass Windows' foreground lock.
/// Maximising also guarantees the window is unoccluded for the CopyFromScreen
/// region grab. CREATE_NO_WINDOW keeps the helper PowerShell from flashing a
/// console.
#[cfg(target_os = "windows")]
fn focus_window(hwnd: u64) {
    use std::os::windows::process::CommandExt;
    let script = format!(
        r#"$sig=@'
using System;
using System.Runtime.InteropServices;
public class FW {{
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool f);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  public static void Go(IntPtr h) {{
    ShowWindow(h, 5); // SW_SHOW — bring up WITHOUT resizing (SW_MAXIMIZE blanks WeChat CEF)
    IntPtr fg = GetForegroundWindow();
    uint pid; uint fgT = GetWindowThreadProcessId(fg, out pid);
    uint cur = GetCurrentThreadId();
    AttachThreadInput(cur, fgT, true);
    BringWindowToTop(h); SetForegroundWindow(h);
    AttachThreadInput(cur, fgT, false);
  }}
}}
'@
Add-Type $sig
[FW]::Go([IntPtr]{hwnd})"#,
        hwnd = hwnd
    );
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &script,
    ]);
    cmd.creation_flags(0x08000000);
    let _ = cmd.output();
}

/// Capture the WeChat window for OCR via a screen-region grab. WeChat 4.x's
/// risk-control blanks PrintWindow (per-window) captures — they come back solid
/// black — but a CopyFromScreen (GDI BitBlt) region grab of the window's
/// on-screen rect is NOT blocked. We foreground + un-minimise the window first
/// (CopyFromScreen captures whatever is displayed at those coords, so the
/// window must be visible), then grab its rect. Returns the PNG plus the
/// refreshed window bounds, so OCR maps hits to correct screen-absolute click
/// points. macOS keeps its backing-store path.
#[cfg(target_os = "windows")]
fn wechat_region_capture(app_name: &str) -> Result<(Vec<u8>, capture::WindowInfo), String> {
    // IMPORTANT: do NOT programmatically focus / maximise / restore the window.
    // WeChat's CEF renderer paints blank gray when its window is shown or resized
    // from a background process — so any SetForegroundWindow/ShowWindow turns the
    // capture into a featureless gray rectangle. Instead capture the window's
    // current on-screen rect exactly as it sits (CopyFromScreen, which WeChat's
    // risk-control does NOT blank, unlike PrintWindow). This relies on WeChat being
    // left visible and unoccluded; if it isn't, the grab will look blank and the
    // caller falls back to WeChat's own built-in screenshot.
    let win = find_app_window(app_name)?;
    let png = capture::capture_region_png(win.x, win.y, win.w, win.h).map_err(|e| e.to_string())?;
    Ok((png, win))
}

/// Heuristic: is a capture essentially blank (solid black / uniform)? WeChat
/// risk-control blanks PrintWindow to pure black; if a region grab ever comes
/// back blank too (window occluded, or stricter anti-capture), we fall back to
/// WeChat's own screenshot. Samples sparsely and reports blank when <1% of
/// sampled pixels are non-black.
#[cfg(target_os = "windows")]
fn looks_blank(png: &[u8]) -> bool {
    match capture::png_to_rgba(png) {
        Ok(img) => {
            let mut nonblack = 0u64;
            let mut total = 0u64;
            for p in img.pixels().step_by(37) {
                total += 1;
                let [r, g, b, _] = p.0;
                if r > 16 || g > 16 || b > 16 {
                    nonblack += 1;
                }
            }
            total == 0 || (nonblack as f64 / total as f64) < 0.01
        }
        // Undecodable → don't assume blank; let OCR try.
        Err(_) => false,
    }
}

/// Read the current clipboard image as PNG bytes (WeChat's built-in screenshot
/// copies the captured region here). Requires STA.
#[cfg(target_os = "windows")]
fn clipboard_image_png() -> Result<Vec<u8>, String> {
    use std::os::windows::process::CommandExt;
    let out_png = std::env::temp_dir().join(format!("rsclaw_clip_{}.png", std::process::id()));
    let script = format!(
        r#"Add-Type -AssemblyName System.Windows.Forms,System.Drawing
$img = [System.Windows.Forms.Clipboard]::GetImage()
if ($img -eq $null) {{ Write-Output 'NOIMG'; exit 1 }}
$img.Save('{path}', [System.Drawing.Imaging.ImageFormat]::Png)
Write-Output 'OK'"#,
        path = out_png.display()
    );
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-STA",
        "-Command",
        &script,
    ]);
    cmd.creation_flags(0x08000000);
    let res = cmd
        .output()
        .map_err(|e| format!("clipboard image spawn failed: {e}"))?;
    if !res.status.success() {
        return Err(format!(
            "clipboard has no image ({})",
            String::from_utf8_lossy(&res.stdout).trim()
        ));
    }
    let bytes = std::fs::read(&out_png).map_err(|e| format!("read clipboard png: {e}"))?;
    let _ = std::fs::remove_file(&out_png);
    Ok(bytes)
}

/// FALLBACK capture via WeChat's OWN built-in screenshot (PC default hotkey
/// Alt+A → drag-select the window → Enter copies to clipboard). Used only when
/// the region grab comes back blank, since WeChat's internal screenshot is
/// never blocked by its own risk-control. Intrusive (steals focus, flashes the
/// selection overlay).
#[cfg(target_os = "windows")]
fn wechat_screenshot_png(win: &capture::WindowInfo) -> Result<Vec<u8>, String> {
    use std::{thread::sleep, time::Duration};
    focus_window(win.id);
    sleep(Duration::from_millis(450));
    let mut e = new_enigo()?;
    // Trigger WeChat's screenshot (Alt held while pressing 'a').
    e.key(Key::Alt, Press)
        .map_err(|err| format!("alt down: {err}"))?;
    e.key(Key::Unicode('a'), Click)
        .map_err(|err| format!("press a: {err}"))?;
    e.key(Key::Alt, Release)
        .map_err(|err| format!("alt up: {err}"))?;
    sleep(Duration::from_millis(700)); // selection overlay appears
    // Drag-select the whole window (real down → move → move → up).
    let pad = 2;
    let (x1, y1) = (win.x + pad, win.y + pad);
    let (x2, y2) = (win.x + win.w as i32 - pad, win.y + win.h as i32 - pad);
    e.move_mouse(x1, y1, Coordinate::Abs)
        .map_err(|err| format!("move start: {err}"))?;
    e.button(Button::Left, Press)
        .map_err(|err| format!("btn down: {err}"))?;
    e.move_mouse((x1 + x2) / 2, (y1 + y2) / 2, Coordinate::Abs)
        .map_err(|err| format!("move mid: {err}"))?;
    e.move_mouse(x2, y2, Coordinate::Abs)
        .map_err(|err| format!("move end: {err}"))?;
    e.button(Button::Left, Release)
        .map_err(|err| format!("btn up: {err}"))?;
    sleep(Duration::from_millis(350));
    // Enter = 完成/复制 → puts the capture on the clipboard, closes the overlay.
    e.key(Key::Return, Click)
        .map_err(|err| format!("enter: {err}"))?;
    sleep(Duration::from_millis(450));
    clipboard_image_png()
}

/// Windows on-device OCR via Windows.Media.Ocr (WinRT) through PowerShell. For
/// WeChat the window is captured via a screen-region grab (PrintWindow is
/// blanked by WeChat risk-control); other apps use the occlusion-proof
/// PrintWindow path. Prints the same JSON shape as macOS:
/// `[{"text","conf","x","y","sx","sy"}]` with x/y 0-1000 window-relative and
/// sx/sy screen-absolute click points. Because the captured image IS the window
/// rect at a known origin, sx/sy land correctly. Capture the WeChat window via
/// the Alt+A built-in screenshot tool (sync, for use inside spawn_blocking /
/// sync OCR context). WeChat's WDA_EXCLUDEFROMCAPTURE blanks all GDI/BitBlt
/// captures — Alt+A is the only reliable path on Windows. Returns (png_bytes,
/// win_x, win_y, win_w, win_h) where the bounds let the OCR driver compute
/// screen-absolute click coordinates.
#[cfg(target_os = "windows")]
fn wechat_altA_screenshot_sync(_app_name: &str) -> Result<(Vec<u8>, i32, i32, u32, u32), String> {
    use std::os::windows::process::CommandExt;
    // Focus WeChat via AttachThreadInput and get its window rect.
    let ps_focus = r#"$sig=@'
using System; using System.Runtime.InteropServices;
public class FWOCR {
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a,uint b,bool f);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h,out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT{public int L,T,R,B;}
  public static string Go(IntPtr h){
    ShowWindow(h,5);
    IntPtr fg=GetForegroundWindow(); uint pid; uint fgT=GetWindowThreadProcessId(fg,out pid);
    uint cur=GetCurrentThreadId(); AttachThreadInput(cur,fgT,true);
    BringWindowToTop(h); SetForegroundWindow(h); AttachThreadInput(cur,fgT,false);
    RECT r; GetWindowRect(h,out r);
    return r.L+","+r.T+","+(r.R-r.L)+","+(r.B-r.T);
  }
}
'@
Add-Type $sig -ErrorAction SilentlyContinue
$p=Get-Process | Where-Object {($_.ProcessName -like '*WeChat*' -or $_.ProcessName -like '*Weixin*') -and $_.MainWindowHandle -ne 0} | Select-Object -First 1
if(-not $p){
  $wx=Get-Process | Where-Object {($_.ProcessName -like '*WeChat*' -or $_.ProcessName -like '*Weixin*')} | Select-Object -First 1
  if($wx){ $path=$wx.Path; if($path){ Start-Process $path } }
  Start-Sleep -Milliseconds 1500
  $p=Get-Process | Where-Object {($_.ProcessName -like '*WeChat*' -or $_.ProcessName -like '*Weixin*') -and $_.MainWindowHandle -ne 0} | Select-Object -First 1
}
if($p){ [FWOCR]::Go($p.MainWindowHandle) } else { Write-Error 'WECHAT_WINDOW_NOT_FOUND'; exit 1 }"#;
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &ps_focus,
    ]);
    cmd.creation_flags(0x08000000);
    let focus = cmd
        .output()
        .map_err(|e| format!("WeChat focus: powershell spawn: {e}"))?;
    if !focus.status.success() {
        return Err(format!(
            "WeChat focus failed: {}",
            String::from_utf8_lossy(&focus.stderr).trim()
        ));
    }
    let s = String::from_utf8_lossy(&focus.stdout);
    let parts: Vec<i64> = s
        .trim()
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    if parts.len() != 4 || parts[2] <= 0 || parts[3] <= 0 {
        return Err(format!(
            "WeChat focus returned invalid window bounds: {s:?}"
        ));
    }
    let wx = parts[0] as i32;
    let wy = parts[1] as i32;
    let ww = parts[2] as u32;
    let wh = parts[3] as u32;
    let cx = wx + (ww as i32 / 2);
    let cy = wy + (wh as i32 / 2);
    std::thread::sleep(std::time::Duration::from_millis(400));
    // Clear clipboard.
    let mut clr = Command::new("powershell");
    clr.args([
        "-NoProfile",
        "-STA",
        "-Command",
        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::Clear()",
    ]);
    clr.creation_flags(0x08000000);
    if let Err(e) = clr.output() {
        tracing::debug!(?e, "Alt+A OCR: failed to clear clipboard before capture");
    }
    // Press Alt+A via enigo.
    let mut enigo = new_enigo()?;
    enigo
        .key(enigo::Key::Alt, enigo::Direction::Press)
        .map_err(|e| format!("alt press: {e}"))?;
    std::thread::sleep(std::time::Duration::from_millis(50));
    enigo
        .key(enigo::Key::Unicode('a'), enigo::Direction::Click)
        .map_err(|e| format!("a: {e}"))?;
    std::thread::sleep(std::time::Duration::from_millis(50));
    enigo
        .key(enigo::Key::Alt, enigo::Direction::Release)
        .map_err(|e| format!("alt release: {e}"))?;
    std::thread::sleep(std::time::Duration::from_millis(900));
    // Hover over window centre so the overlay selects the WeChat window.
    let (lx, ly) = scale_for_input(cx as u32, cy as u32);
    enigo
        .move_mouse(lx, ly, enigo::Coordinate::Abs)
        .map_err(|e| format!("mouse move: {e}"))?;
    std::thread::sleep(std::time::Duration::from_millis(600));
    // Confirm screenshot.
    enigo
        .key(enigo::Key::Return, enigo::Direction::Click)
        .map_err(|e| format!("enter: {e}"))?;
    std::thread::sleep(std::time::Duration::from_millis(1000));
    // Read PNG from clipboard.
    let tmp = PrivateTempFile::create("rsclaw-ocr-alta-", ".png")?;
    let ps_read = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         Add-Type -AssemblyName System.Drawing; \
         $img=[System.Windows.Forms.Clipboard]::GetImage(); \
         if($img -eq $null){{ Write-Error 'CLIPBOARD_EMPTY'; exit 1 }}; \
         $img.Save('{}',[System.Drawing.Imaging.ImageFormat]::Png); 'ok'",
        tmp.path().display()
    );
    let mut ps_cmd = Command::new("powershell");
    ps_cmd.args(["-NoProfile", "-STA", "-Command", &ps_read]);
    ps_cmd.creation_flags(0x08000000);
    match ps_cmd.output() {
        Ok(out) if out.status.success() => match std::fs::read(tmp.path()) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    Err("Alt+A OCR screenshot: empty clipboard image".to_string())
                } else {
                    Ok((bytes, wx, wy, ww, wh))
                }
            }
            Err(e) => Err(format!("Alt+A OCR screenshot: read temp: {e}")),
        },
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!(
                "Alt+A OCR screenshot: clipboard empty or PS failed: {stderr}"
            ))
        }
        Err(e) => Err(format!("Alt+A OCR screenshot: powershell spawn: {e}")),
    }
}

#[cfg(target_os = "windows")]
fn ocr_window(app_name: &str) -> Result<String, String> {
    let lname = app_name.to_lowercase();
    let (png, wx, wy, ww, wh) = if lname.contains("wechat") || lname.contains("weixin") {
        // WeChat uses WDA_EXCLUDEFROMCAPTURE — GDI/BitBlt captures come back blank.
        // Use WeChat's built-in Alt+A screenshot (captures itself, saves to clipboard).
        let (bytes, x, y, w, h) = wechat_altA_screenshot_sync(app_name)?;
        (bytes, x, y, w, h)
    } else {
        let win = find_app_window(app_name)?;
        let png = capture::capture_window_png(win.id).map_err(|e| e.to_string())?;
        (png, win.x, win.y, win.w, win.h)
    };
    let tmp = PrivateTempFile::create("rsclaw-ocr-win-", ".png")?;
    std::fs::write(tmp.path(), &png).map_err(|e| format!("write OCR temp PNG: {e}"))?;
    // Write the OCR script to a temp .ps1 (avoids -Command quoting hell) and run
    // it STA (WinRT requires it). Args: <png> <wx> <wy> <ww> <wh>.
    let ps = PrivateTempFile::create("rsclaw-ocr-win-", ".ps1")?;
    std::fs::write(ps.path(), OCR_WIN_PS1).map_err(|e| format!("write OCR script: {e}"))?;
    let mut ocr_cmd = Command::new("powershell");
    ocr_cmd
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-STA", "-File"])
        .arg(ps.path())
        .arg(tmp.path())
        .arg(wx.to_string())
        .arg(wy.to_string())
        .arg(ww.to_string())
        .arg(wh.to_string());
    {
        // CREATE_NO_WINDOW: don't allocate a console window (no screen flash on
        // every monitor_tick).
        use std::os::windows::process::CommandExt;
        ocr_cmd.creation_flags(0x08000000);
    }
    let out = ocr_cmd
        .output()
        .map_err(|e| format!("powershell OCR spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Windows OCR failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Windows.Media.Ocr driver (PowerShell, STA). argv: png, wx, wy, ww, wh
/// (window screen bounds in logical px). Emits
/// `[{"text","conf","x","y","sx","sy"}]`: x/y are 0-1000 window-relative line
/// centres; sx/sy are screen-absolute click points (window origin + relative *
/// window size). conf is 100 (Windows OCR has no per-line confidence). The
/// captured PNG IS the window, so we normalise by the image's own pixel size.
#[cfg(target_os = "windows")]
const OCR_WIN_PS1: &str = r#"param([string]$img,[int]$wx,[int]$wy,[int]$ww,[int]$wh)
$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
[void][Windows.Storage.StorageFile,Windows.Storage,ContentType=WindowsRuntime]
[void][Windows.Media.Ocr.OcrEngine,Windows.Foundation,ContentType=WindowsRuntime]
[void][Windows.Graphics.Imaging.BitmapDecoder,Windows.Foundation,ContentType=WindowsRuntime]
$asTask=([System.WindowsRuntimeSystemExtensions].GetMethods()|?{$_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'})[0]
function Await($op,$t){ $m=$asTask.MakeGenericMethod($t); $tk=$m.Invoke($null,@($op)); $tk.Wait(); $tk.Result }
$file=Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($img)) ([Windows.Storage.StorageFile])
$stream=Await ($file.OpenAsync([Windows.Storage.FileAccessMode]::Read)) ([Windows.Storage.Streams.IRandomAccessStream])
$dec=Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder])
$sb=Await ($dec.GetSoftwareBitmapAsync()) ([Windows.Graphics.Imaging.SoftwareBitmap])
$iw=[double]$dec.PixelWidth; $ih=[double]$dec.PixelHeight
$eng=[Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
if(-not $eng){ $eng=[Windows.Media.Ocr.OcrEngine]::TryCreateFromLanguage(([Windows.Media.Ocr.OcrEngine]::AvailableRecognizerLanguages)[0]) }
$res=Await ($eng.RecognizeAsync($sb)) ([Windows.Media.Ocr.OcrResult])
$out=@()
foreach($l in $res.Lines){
  $minx=1e9;$miny=1e9;$maxx=0;$maxy=0
  foreach($w in $l.Words){ $r=$w.BoundingRect; if($r.X -lt $minx){$minx=$r.X}; if($r.Y -lt $miny){$miny=$r.Y}; if(($r.X+$r.Width) -gt $maxx){$maxx=$r.X+$r.Width}; if(($r.Y+$r.Height) -gt $maxy){$maxy=$r.Y+$r.Height} }
  if($maxx -le 0){continue}
  $cx=($minx+$maxx)/2.0; $cy=($miny+$maxy)/2.0
  $rx=[int][math]::Round($cx/$iw*1000.0); $ry=[int][math]::Round($cy/$ih*1000.0)
  # ww<=0 => full-screen capture: the image pixels ARE screen coordinates.
  if($ww -le 0){ $sx=[int]$cx; $sy=[int]$cy } else { $sx=[int]($wx + $cx/$iw*$ww); $sy=[int]($wy + $cy/$ih*$wh) }
  $out += [pscustomobject]@{ text=$l.Text; conf=100; x=$rx; y=$ry; sx=$sx; sy=$sy }
}
$out | ConvertTo-Json -Compress -Depth 3
"#;

/// macOS on-device OCR via the Vision framework (`VNRecognizeTextRequest`),
/// called through objc2 FFI — the in-tree replacement for the previous
/// python3 + pyobjc Vision shell-out. Captures the app's frontmost window by id
/// (occlusion-proof `screencapture -l`), runs text recognition, and emits the
/// same JSON shape as the Windows path: `[{"text","conf","x","y","sx","sy"}]`
/// where x/y are 0-1000 window-relative line centres and sx/sy are
/// screen-absolute (logical-point) click points.
#[cfg(target_os = "macos")]
fn ocr_window(app_name: &str) -> Result<String, String> {
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };

    let win = find_app_window(app_name)?;
    let png = capture::capture_window_png(win.id).map_err(|e| e.to_string())?;
    if png.is_empty() {
        return Ok("[]".to_string());
    }
    // Vision reads the image from a file URL (avoids constructing a CGImage from
    // raw bytes). Keep a uniquely named temp file alive through the request.
    let tmp = PrivateTempFile::create("rsclaw-ocr-", ".png")?;
    std::fs::write(tmp.path(), &png).map_err(|e| format!("write OCR temp PNG: {e}"))?;

    let result = (|| -> Result<String, String> {
        let path = NSString::from_str(&tmp.path().to_string_lossy());
        let url = NSURL::fileURLWithPath(&path);
        let options = NSDictionary::new();
        let handler = unsafe {
            VNImageRequestHandler::initWithURL_options(
                VNImageRequestHandler::alloc(),
                &url,
                &options,
            )
        };

        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        request.setUsesLanguageCorrection(true);
        // The chat apps this drives are Simplified Chinese + English.
        let zh = NSString::from_str("zh-Hans");
        let en = NSString::from_str("en-US");
        let langs = NSArray::from_slice(&[&*zh, &*en]);
        request.setRecognitionLanguages(&langs);

        let req_ref: &VNRequest = &request;
        let requests = NSArray::from_slice(&[req_ref]);
        handler
            .performRequests_error(&requests)
            .map_err(|e| format!("Vision performRequests failed: {e}"))?;

        let observations = match request.results() {
            Some(o) => o,
            None => return Ok("[]".to_string()),
        };

        let (wx, wy, ww, wh) = (win.x as f64, win.y as f64, win.w as f64, win.h as f64);
        let mut out: Vec<serde_json::Value> = Vec::new();
        for obs in observations.iter() {
            let candidates = obs.topCandidates(1);
            let Some(top) = candidates.firstObject() else {
                continue;
            };
            let text = top.string().to_string();
            if text.trim().is_empty() {
                continue;
            }
            let conf = (top.confidence() as f64 * 100.0).round() as i64;
            // boundingBox is normalised [0,1] with a BOTTOM-LEFT origin.
            let bb = unsafe { obs.boundingBox() };
            let midx = bb.origin.x + bb.size.width / 2.0;
            let midy = bb.origin.y + bb.size.height / 2.0;
            // Flip Y to a top-left origin for both the 0-1000 relative coord and
            // the screen-absolute click point.
            let top_y = 1.0 - midy;
            let rx = (midx * 1000.0).round() as i64;
            let ry = (top_y * 1000.0).round() as i64;
            let sx = (wx + midx * ww).round() as i64;
            let sy = (wy + top_y * wh).round() as i64;
            out.push(serde_json::json!({
                "text": text,
                "conf": conf,
                "x": rx,
                "y": ry,
                "sx": sx,
                "sy": sy,
            }));
        }
        Ok(serde_json::Value::Array(out).to_string())
    })();

    result
}

/// One OCR hit in pixel coordinates relative to the image that was OCR'd
/// (the cropped region, or the full image when no rect was given). Confidence
/// is normalised to 0-1.
struct OcrHit {
    text: String,
    confidence: f64,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// OCR an in-memory PNG with the native on-device engine (macOS Vision,
/// Windows.Media.Ocr on Windows) — no network round trip, so it is fast enough
/// for per-keystroke UI confirmation. When `rect` is `Some((x, y, w, h))` the
/// image is cropped to that pixel region first (clamped to the image bounds);
/// the OCR then runs on the crop only. Returns JSON
/// `{"ok":true,"lines":[{"text","confidence","x","y","width","height"}]}` where
/// `confidence` is 0-1 and `x/y/width/height` are pixel coordinates in the
/// ORIGINAL (uncropped) image. This is the host backend for the plugin's
/// `android_ocr_region` fast path.
pub fn ocr_image_region(png: &[u8], rect: Option<(u32, u32, u32, u32)>) -> Result<String, String> {
    use image::GenericImageView;
    let img = image::load_from_memory(png)
        .map_err(|e| format!("ocr_image_region: decode PNG: {e}"))?;
    let rgb = img.to_rgb8();
    let iw = rgb.width();
    let ih = rgb.height();
    let (crop_x, crop_y, region) = match rect {
        Some((rx, ry, rw, rh)) => {
            let rx = rx.min(iw.saturating_sub(1));
            let ry = ry.min(ih.saturating_sub(1));
            let rw = rw.min(iw.saturating_sub(rx)).max(1);
            let rh = rh.min(ih.saturating_sub(ry)).max(1);
            (rx, ry, rgb.view(rx, ry, rw, rh).to_image())
        }
        None => (0, 0, rgb.clone()),
    };
    let (rw, rh) = (region.width(), region.height());
    let tmp = std::env::temp_dir().join(format!("rsclaw_ocr_region_{}.png", std::process::id()));
    region
        .save(&tmp)
        .map_err(|e| format!("ocr_image_region: write temp PNG: {e}"))?;

    let result = (|| -> Result<String, String> {
        let hits = ocr_file_hits(&tmp, rw, rh)?;
        let mut lines: Vec<serde_json::Value> = Vec::new();
        for hit in hits {
            if hit.text.trim().is_empty() {
                continue;
            }
            lines.push(serde_json::json!({
                "text": hit.text,
                "confidence": hit.confidence,
                "x": (f64::from(crop_x) + hit.x).round() as i64,
                "y": (f64::from(crop_y) + hit.y).round() as i64,
                "width": hit.width.round() as i64,
                "height": hit.height.round() as i64,
            }));
        }
        Ok(serde_json::json!({ "ok": true, "lines": lines }).to_string())
    })();

    let _ = std::fs::remove_file(&tmp);
    result
}

/// macOS Vision OCR of a PNG file already on disk. `iw`/`ih` are the image's
/// pixel dimensions, used to denormalise Vision's [0,1] bottom-left bounding
/// boxes into top-left pixel coordinates.
#[cfg(target_os = "macos")]
fn ocr_file_hits(path: &std::path::Path, iw: u32, ih: u32) -> Result<Vec<OcrHit>, String> {
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };

    let path_str = NSString::from_str(&path.to_string_lossy());
    let url = NSURL::fileURLWithPath(&path_str);
    let options = NSDictionary::new();
    let handler = unsafe {
        VNImageRequestHandler::initWithURL_options(VNImageRequestHandler::alloc(), &url, &options)
    };

    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    let zh = NSString::from_str("zh-Hans");
    let en = NSString::from_str("en-US");
    let langs = NSArray::from_slice(&[&*zh, &*en]);
    request.setRecognitionLanguages(&langs);

    let req_ref: &VNRequest = &request;
    let requests = NSArray::from_slice(&[req_ref]);
    handler
        .performRequests_error(&requests)
        .map_err(|e| format!("Vision performRequests failed: {e}"))?;

    let observations = match request.results() {
        Some(o) => o,
        None => return Ok(Vec::new()),
    };

    let (iw, ih) = (f64::from(iw), f64::from(ih));
    let mut hits = Vec::new();
    for obs in observations.iter() {
        let candidates = obs.topCandidates(1);
        let Some(top) = candidates.firstObject() else {
            continue;
        };
        let text = top.string().to_string();
        if text.trim().is_empty() {
            continue;
        }
        let bb = unsafe { obs.boundingBox() };
        let width = bb.size.width * iw;
        let height = bb.size.height * ih;
        let x = bb.origin.x * iw;
        // Vision's boundingBox origin is bottom-left; flip to a top-left y.
        let y = (1.0 - bb.origin.y - bb.size.height) * ih;
        hits.push(OcrHit {
            text,
            confidence: top.confidence() as f64,
            x,
            y,
            width,
            height,
        });
    }
    Ok(hits)
}

/// Windows.Media.Ocr (WinRT via PowerShell) of a PNG file on disk. Runs the
/// shared OCR_WIN_PS1 in full-image mode (window bounds 0) so the emitted
/// sx/sy are pixel centres within the image. Windows OCR exposes no per-line
/// size, so width/height are reported as 0.
#[cfg(target_os = "windows")]
fn ocr_file_hits(path: &std::path::Path, _iw: u32, _ih: u32) -> Result<Vec<OcrHit>, String> {
    let ps = std::env::temp_dir().join("rsclaw_ocr_region.ps1");
    let _ = std::fs::write(&ps, OCR_WIN_PS1);
    let mut ocr_cmd = Command::new("powershell");
    ocr_cmd
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-STA", "-File"])
        .arg(&ps)
        .arg(path)
        .arg("0")
        .arg("0")
        .arg("0")
        .arg("0");
    {
        use std::os::windows::process::CommandExt;
        ocr_cmd.creation_flags(0x08000000);
    }
    let out = ocr_cmd
        .output()
        .map_err(|e| format!("powershell OCR spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Windows OCR failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.is_empty() || stdout == "null" {
        return Ok(Vec::new());
    }
    let rows: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .map_err(|e| format!("Windows OCR parse failed: {e}; out={stdout}"))?;
    let mut hits = Vec::new();
    for row in rows {
        let text = row.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if text.trim().is_empty() {
            continue;
        }
        let conf = row.get("conf").and_then(|v| v.as_f64()).unwrap_or(100.0);
        let sx = row.get("sx").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let sy = row.get("sy").and_then(|v| v.as_f64()).unwrap_or(0.0);
        hits.push(OcrHit {
            text,
            confidence: conf / 100.0,
            x: sx,
            y: sy,
            width: 0.0,
            height: 0.0,
        });
    }
    Ok(hits)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn ocr_file_hits(_path: &std::path::Path, _iw: u32, _ih: u32) -> Result<Vec<OcrHit>, String> {
    Err("ocr_image_region only implemented on macOS and Windows".to_string())
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
#[cfg(target_os = "macos")]
fn cgwindow_fallback(owner_name: &str) -> Result<String, String> {
    use core_foundation::{
        base::{CFType, TCFType},
        number::CFNumber,
        string::CFString,
    };

    const K_CG_WINDOW_LIST_OPTION_ALL: u32 = 0;
    const K_CG_NULL_WINDOW_ID: u32 = 0;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(
            option: u32,
            relative_to_window: u32,
        ) -> core_foundation::array::CFArrayRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(arr: core_foundation::array::CFArrayRef) -> isize;
        fn CFArrayGetValueAtIndex(
            arr: core_foundation::array::CFArrayRef,
            idx: isize,
        ) -> *const std::ffi::c_void;
        fn CFDictionaryGetValue(
            dict: core_foundation::dictionary::CFDictionaryRef,
            key: *const std::ffi::c_void,
        ) -> *const std::ffi::c_void;
        fn CFRelease(cf: *const std::ffi::c_void);
    }

    /// Look up a value in a raw CFDictionaryRef by key string.
    fn dict_val(dict: core_foundation::dictionary::CFDictionaryRef, key: &str) -> Option<CFType> {
        let cf_key = CFString::new(key);
        let val = unsafe { CFDictionaryGetValue(dict, cf_key.as_concrete_TypeRef() as *const _) };
        if val.is_null() {
            None
        } else {
            Some(unsafe { CFType::wrap_under_get_rule(val as core_foundation::base::CFTypeRef) })
        }
    }

    fn dict_num(dict: core_foundation::dictionary::CFDictionaryRef, key: &str) -> i64 {
        dict_val(dict, key)
            .and_then(|cf| cf.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
            .unwrap_or(0)
    }

    fn dict_str(dict: core_foundation::dictionary::CFDictionaryRef, key: &str) -> String {
        dict_val(dict, key)
            .and_then(|cf| cf.downcast::<CFString>())
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    fn dict_dict_ref(
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

    let cf_array_ref =
        unsafe { CGWindowListCopyWindowInfo(K_CG_WINDOW_LIST_OPTION_ALL, K_CG_NULL_WINDOW_ID) };
    if cf_array_ref.is_null() {
        return Err("cgwindow_fallback: CGWindowListCopyWindowInfo returned NULL".to_string());
    }
    let count = unsafe { CFArrayGetCount(cf_array_ref) };
    let mut best: Option<(i32, i32, u32, u32)> = None;
    let mut best_area: u64 = 0;
    for i in 0..count {
        let dict_ref = unsafe { CFArrayGetValueAtIndex(cf_array_ref, i) }
            as core_foundation::dictionary::CFDictionaryRef;
        if dict_ref.is_null() {
            continue;
        }
        if dict_str(dict_ref, "kCGWindowOwnerName") != owner_name {
            continue;
        }
        let bounds_ref = match dict_dict_ref(dict_ref, "kCGWindowBounds") {
            Some(b) => b,
            None => continue,
        };
        let x = dict_num(bounds_ref, "X") as i32;
        let y = dict_num(bounds_ref, "Y") as i32;
        let w = dict_num(bounds_ref, "Width") as u32;
        let h = dict_num(bounds_ref, "Height") as u32;
        if w < 200 || h < 200 {
            continue;
        }
        let area = w as u64 * h as u64;
        if area > best_area {
            best_area = area;
            best = Some((x, y, w, h));
        }
    }
    // Release the array (Create Rule — we own it).
    unsafe { CFRelease(cf_array_ref as *const std::ffi::c_void) };
    match best {
        Some((x, y, w, h)) => Ok(format!("{{\"x\":{x},\"y\":{y},\"w\":{w},\"h\":{h}}}")),
        None => Err(format!(
            "cgwindow_fallback: no {owner_name} window found via CGWindowList"
        )),
    }
}

#[cfg(not(target_os = "macos"))]
fn cgwindow_fallback(owner_name: &str) -> Result<String, String> {
    Err(format!(
        "cgwindow_fallback: not supported on this platform (owner: {owner_name})"
    ))
}

// ---------------------------------------------------------------------------
// Inherent helpers
// ---------------------------------------------------------------------------

impl NativeDesktopSession {
    /// Windows: capture WeChat via its OWN built-in screenshot (Alt+A), routing
    /// pixels through the clipboard. Avoids GDI/PrintWindow (which WeChat
    /// blanks via WDA_EXCLUDEFROMCAPTURE) and the screen-capture
    /// risk-control that white-screens / force-logs-out the client.
    async fn wechat_builtin_screenshot(&self, bundle_id: &str) -> Result<String, String> {
        // 1. Find WeChat's HWND and window bounds, then force-focus using
        //    AttachThreadInput (plain SetForegroundWindow is ignored from a background
        //    gateway process).
        let bl = bundle_id.to_lowercase();
        let proc_name = if bl.contains("weixin") {
            "Weixin"
        } else {
            "WeChat"
        };
        let ps_focus = format!(
            r#"$sig=@'
using System; using System.Runtime.InteropServices;
public class FW2 {{
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a,uint b,bool f);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h,out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT{{public int L,T,R,B;}}
  public static string Go(IntPtr h){{
    ShowWindow(h,5);
    IntPtr fg=GetForegroundWindow(); uint pid; uint fgT=GetWindowThreadProcessId(fg,out pid);
    uint cur=GetCurrentThreadId(); AttachThreadInput(cur,fgT,true);
    BringWindowToTop(h); SetForegroundWindow(h); AttachThreadInput(cur,fgT,false);
    RECT r; GetWindowRect(h,out r);
    return r.L+","+r.T+","+(r.R-r.L)+","+(r.B-r.T);
  }}
}}
'@
Add-Type $sig -ErrorAction SilentlyContinue
$p=Get-Process | Where-Object {{$_.ProcessName -like '*{proc}*' -and $_.MainWindowHandle -ne 0}} | Select-Object -First 1
if(-not $p){{
  # WeChat is in the system tray (no visible window). Launch the exe to restore it.
  $wx=Get-Process | Where-Object {{$_.ProcessName -like '*{proc}*'}} | Select-Object -First 1
  if($wx){{
    $path=$wx.Path
    if($path){{ Start-Process $path }}
  }}
  Start-Sleep -Milliseconds 1500
  $p=Get-Process | Where-Object {{$_.ProcessName -like '*{proc}*' -and $_.MainWindowHandle -ne 0}} | Select-Object -First 1
}}
if($p){{ [FW2]::Go($p.MainWindowHandle) }} else {{ "0,0,1200,800" }}"#,
            proc = proc_name
        );
        let (cx, cy) = tokio::task::spawn_blocking({
            let ps = ps_focus.clone();
            move || -> (u32, u32) {
                let mut cmd = Command::new("powershell");
                cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &ps]);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                let out = cmd.output().unwrap_or(std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: vec![],
                    stderr: vec![],
                });
                let s = String::from_utf8_lossy(&out.stdout);
                let parts: Vec<i64> = s
                    .trim()
                    .split(',')
                    .filter_map(|p| p.trim().parse().ok())
                    .collect();
                if parts.len() == 4 {
                    let x = parts[0].max(0) as u32;
                    let y = parts[1].max(0) as u32;
                    let w = parts[2].max(1) as u32;
                    let h = parts[3].max(1) as u32;
                    (x + w / 2, y + h / 2)
                } else {
                    (700, 450)
                }
            }
        })
        .await
        .unwrap_or((700, 450));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        // 2. Clear clipboard so we don't read a stale image.
        let _ = self.clipboard_set("").await;
        // 3. Trigger WeChat screenshot overlay: Alt+A.
        self.key_press("a", &["alt".to_string()]).await?;
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        // 4. Move cursor over the WeChat window center so the overlay hover-selects it.
        let _ = self.mouse_move(cx, cy).await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        // 5. Press Enter to confirm the highlighted window capture → clipboard.
        let _ = self.key_press("Return", &[]).await;
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        // 6. Read the screenshot from the clipboard.
        self.clipboard_get_image().await
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
                // Frontmost check (System Events) — used both as a fast-path
                // (skip the whole activation dance when already frontmost, the
                // common case in a monitor loop) and as the verify poll.
                let check_script = format!(
                    r#"tell application "System Events"
    return frontmost of (first process whose bundle identifier is "{}")
end tell"#,
                    bundle_id.replace('"', r#"\""#)
                );
                let is_frontmost = || {
                    matches!(
                        Command::new("osascript").args(["-e", &check_script]).output(),
                        Ok(out) if out.status.success()
                            && String::from_utf8_lossy(&out.stdout).trim().eq_ignore_ascii_case("true")
                    )
                };

                // FAST PATH: already frontmost → return immediately (~30ms).
                // Previously every call paid 500ms + 300ms + up to 5×300ms of
                // fixed sleeps (~2-5s) even when the app was already active —
                // the dominant cost in the monitor loop.
                if is_frontmost() {
                    return Ok("ok".to_string());
                }

                // Three-layer fallback (matches wechat_agent.py):
                // 1. open -b (system-level launchctl, most reliable)
                // 2. activate via AppleScript
                // 3. set frontmost via System Events
                let _ = Command::new("open").args(["-b", &bundle_id]).output();
                std::thread::sleep(std::time::Duration::from_millis(250));

                let script_activate = format!(
                    r#"tell application id "{}" to activate"#,
                    bundle_id.replace('"', r#"\""#)
                );
                let _ = Command::new("osascript")
                    .args(["-e", &script_activate])
                    .output();

                let script_frontmost = format!(
                    r#"tell application "System Events"
    set frontmost of (first process whose bundle identifier is "{}") to true
end tell"#,
                    bundle_id.replace('"', r#"\""#)
                );
                let _ = Command::new("osascript")
                    .args(["-e", &script_frontmost])
                    .output();

                // Verify frontmost up to 4 times (100ms apart), exit as soon as
                // confirmed.
                for _ in 0..4 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if is_frontmost() {
                        return Ok("ok".to_string());
                    }
                    let _ = Command::new("open").args(["-b", &bundle_id]).output();
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
                #[cfg(target_os = "windows")]
                {
                    // Full-screen-capture model: WeChat's OCR is a full-screen grab and
                    // the plugin's 0-1000 coords are relative to the whole screen, so the
                    // "main window" bounds the plugin should use ARE the full virtual
                    // screen. Return its rect (origin + size) so relative→screen maps 1:1.
                    let (vx, vy, vw, vh) = virtual_screen_rect();
                    return Ok(format!(
                        "{{\"x\":{vx},\"y\":{vy},\"w\":{vw},\"h\":{vh}}}"
                    ));
                }
                #[cfg(not(target_os = "windows"))]
                {
                    return Err("get_main_window: not yet implemented on this platform".to_string());
                }
            }
        })
        .await
        .map_err(|e| format!("get_main_window join failed: {e}"))?
    }

    async fn screenshot_window(&self, bundle_id: &str) -> Result<String, String> {
        // Windows WeChat blanks GDI/PrintWindow captures (WDA_EXCLUDEFROMCAPTURE),
        // and repeated GDI screen capture trips its risk-control (white-screen /
        // forced logout). Use WeChat's OWN built-in screenshot (Alt+A → clipboard),
        // which can capture itself and is a sanctioned user action.
        if cfg!(target_os = "windows") {
            let bl = bundle_id.to_lowercase();
            if bl.contains("wechat") || bl.contains("weixin") || bl.contains("xinwechat") {
                match self.wechat_builtin_screenshot(bundle_id).await {
                    Ok(img) => return Ok(img),
                    Err(e) => warn!("wechat builtin screenshot failed ({e}); falling back"),
                }
            }
        }
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

    async fn region_has_color(
        &self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        r: u32,
        g: u32,
        b: u32,
        tolerance: u32,
        min_count: u32,
    ) -> Result<String, String> {
        let (count, total) =
            tokio::task::spawn_blocking(move || region_color_count(x, y, w, h, r, g, b, tolerance))
                .await
                .map_err(|e| format!("region_has_color join failed: {e}"))??;
        let ratio = if total > 0 {
            count as f64 / total as f64
        } else {
            0.0
        };
        let hit = count >= min_count;
        Ok(format!(
            "{{\"hit\":{hit},\"count\":{count},\"total\":{total},\"ratio\":{ratio:.4}}}"
        ))
    }

    async fn ocr_window(&self, bundle_id: &str) -> Result<String, String> {
        let app_name = bundle_to_app_name(bundle_id);
        tokio::task::spawn_blocking(move || ocr_window(&app_name))
            .await
            .map_err(|e| format!("ocr_window join failed: {e}"))?
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
            #[cfg(target_os = "macos")]
            {
                // macOS: post raw CGEvents for reliable multi-step dragging.
                // enigo's move_mouse does not produce the exact CGEvent sequence
                // that WeChat's screenshot overlay requires.
                use std::{thread::sleep, time::Duration};

                use core_graphics::{
                    event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton},
                    event_source::{CGEventSource, CGEventSourceStateID},
                    geometry::CGPoint,
                };

                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .map_err(|()| "mouse_drag: CGEventSource::new failed".to_string())?;

                let start = CGPoint::new(x1 as f64, y1 as f64);
                let end = CGPoint::new(x2 as f64, y2 as f64);
                let steps: u32 = 20;

                // mouseDown at start
                let down = CGEvent::new_mouse_event(
                    source.clone(),
                    CGEventType::LeftMouseDown,
                    start,
                    CGMouseButton::Left,
                )
                .map_err(|()| "mouse_drag: new_mouse_event(LeftMouseDown) failed".to_string())?;
                down.post(CGEventTapLocation::HID);
                sleep(Duration::from_millis(100));

                // 20 interpolated mouseDragged events
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    let cx = x1 as f64 + (x2 as f64 - x1 as f64) * t;
                    let cy = y1 as f64 + (y2 as f64 - y1 as f64) * t;
                    let pt = CGPoint::new(cx, cy);
                    let drag = CGEvent::new_mouse_event(
                        source.clone(),
                        CGEventType::LeftMouseDragged,
                        pt,
                        CGMouseButton::Left,
                    )
                    .map_err(|()| {
                        format!("mouse_drag: new_mouse_event(LeftMouseDragged) step {i} failed")
                    })?;
                    drag.post(CGEventTapLocation::HID);
                    sleep(Duration::from_millis(25));
                }

                // mouseUp at end
                sleep(Duration::from_millis(100));
                let up = CGEvent::new_mouse_event(
                    source,
                    CGEventType::LeftMouseUp,
                    end,
                    CGMouseButton::Left,
                )
                .map_err(|()| "mouse_drag: new_mouse_event(LeftMouseUp) failed".to_string())?;
                up.post(CGEventTapLocation::HID);

                return Ok("ok".to_string());
            }
            #[cfg(not(target_os = "macos"))]
            {
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
            // macOS: WeChat 4.x's CEF/radium view IGNORES enigo's scroll event
            // (decorated with an event source + flags) — the message list never
            // moves (verified: screenshot hash identical before/after). A RAW
            // CGScrollWheelEvent does move it. Match enigo's vertical sign
            // convention (wheel1 = -clicks) so callers are unaffected: a
            // negative `clicks` scrolls UP toward older content.
            #[cfg(target_os = "macos")]
            {
                use core_graphics::{
                    event::{CGEvent, CGEventTapLocation, ScrollEventUnit},
                    event_source::{CGEventSource, CGEventSourceStateID},
                };
                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .map_err(|()| "scroll: CGEventSource::new failed".to_string())?;
                let event =
                    CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 1, -clicks, 0, 0)
                        .map_err(|()| "scroll: new_scroll_event failed".to_string())?;
                event.post(CGEventTapLocation::HID);
                return Ok("ok".to_string());
            }
            #[cfg(not(target_os = "macos"))]
            {
                let mut enigo = new_enigo()?;
                let axis = Axis::Vertical;
                enigo
                    .scroll(clicks, axis)
                    .map_err(|e| format!("scroll: {e}"))?;
                Ok("ok".to_string())
            }
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
                    // The (mac-authored) callers pass "command" for shortcuts that on
                    // Windows/Linux are Ctrl (Cmd+V→Ctrl+V, Cmd+A→Ctrl+A). enigo's
                    // Meta = the Win key here, which would fire Win+V (clipboard
                    // history) etc. Remap command/meta/super → Control off macOS.
                    let ml = m.trim().to_lowercase();
                    let m_eff = match ml.as_str() {
                        "command" | "cmd" | "meta" | "super" | "win" => "control",
                        other => other,
                    };
                    let mk = parse_key(m_eff).ok_or_else(|| format!("unknown modifier: {m}"))?;
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
                // Write the text as UTF-8 to a temp file and have PowerShell read it
                // back — passing Chinese through a -Command arg mangles it to '?'
                // (and then Set-Clipboard fails). Run STA (clipboard needs it).
                let tmp = std::env::temp_dir()
                    .join(format!("rsclaw_clipset_{}.txt", std::process::id()));
                std::fs::write(&tmp, text.as_bytes())
                    .map_err(|e| format!("clipboard_set temp write: {e}"))?;
                // Retry Set-Clipboard a few times: when a RustDesk/remote viewer is
                // connected its clipboard sync intermittently holds the clipboard,
                // making Set-Clipboard throw "failed to open clipboard".
                let ps = format!(
                    "$t=[System.IO.File]::ReadAllText('{}',[System.Text.Encoding]::UTF8); \
                     $ok=$false; \
                     for($i=0;$i -lt 10;$i++){{ try{{ Set-Clipboard -Value $t -ErrorAction Stop; $ok=$true; break }} \
                     catch{{ Start-Sleep -Milliseconds 250 }} }} \
                     if(-not $ok){{ Write-Error 'clipboard busy after retries'; exit 1 }}",
                    tmp.display()
                );
                #[allow(unused_mut)]
                let mut ps_cmd = Command::new("powershell");
                ps_cmd.args(["-NoProfile", "-STA", "-Command", &ps]);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    ps_cmd.creation_flags(0x08000000);
                }
                let r = ps_cmd.output();
                let _ = std::fs::remove_file(&tmp);
                match r {
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
                match ps_cmd.output() {
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
                match ps_cmd.output() {
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
                // Use AppleScript to read the clipboard as PNG data and write to
                // a temp file. This replaces the previous python3 + AppKit
                // shell-out with a zero-dependency osascript call.
                let script = format!(
                    r#"try
    set img to the clipboard as <<class PNGf>>
    set f to open for access POSIX file "{tmp}" with write permission
    write img to f
    close access f
    return "ok"
on error
    return "NOIMG"
end try"#,
                    tmp = tmp
                );
                match Command::new("osascript").args(["-e", &script]).output() {
                    Ok(out) if out.status.success() => {
                        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if stdout.contains("NOIMG") {
                            return Err(
                                "clipboard_get_image: clipboard has no image \
                                 (screenshot likely failed)"
                                    .to_string(),
                            );
                        }
                        match std::fs::read(&tmp) {
                            Ok(bytes) => {
                                let _ = std::fs::remove_file(&tmp);
                                if bytes.is_empty() {
                                    return Err("clipboard_get_image: empty image".to_string());
                                }
                                let b64 =
                                    base64::engine::general_purpose::STANDARD.encode(&bytes);
                                Ok(format!("data:image/png;base64,{b64}"))
                            }
                            Err(e) => Err(format!("clipboard_get_image: read temp file: {e}")),
                        }
                    }
                    Ok(out) => Err(format!(
                        "clipboard_get_image: osascript failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )),
                    Err(e) => Err(format!("clipboard_get_image: osascript spawn failed: {e}")),
                }
            } else if cfg!(target_os = "windows") {
                // Windows: WeChat's built-in screenshot (Alt+A) copies to the
                // clipboard as an image. Read it via WinForms Clipboard.GetImage
                // (needs an STA thread) and PNG-encode. Using WeChat's own capture
                // avoids the GDI/PrintWindow path that WeChat blanks via
                // WDA_EXCLUDEFROMCAPTURE.
                let tmp = std::env::temp_dir()
                    .join(format!("rsclaw_cb_{}.png", std::process::id()));
                let ps = format!(
                    "Add-Type -AssemblyName System.Windows.Forms; \
                     Add-Type -AssemblyName System.Drawing; \
                     $img=[System.Windows.Forms.Clipboard]::GetImage(); \
                     if($img -eq $null){{ Write-Error 'CLIPBOARD_EMPTY'; exit 1 }}; \
                     $img.Save('{}',[System.Drawing.Imaging.ImageFormat]::Png); \
                     'ok'",
                    tmp.display()
                );
                #[allow(unused_mut)]
                let mut ps_cmd = Command::new("powershell");
                ps_cmd.args(["-NoProfile", "-STA", "-Command", &ps]);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    ps_cmd.creation_flags(0x08000000);
                }
                match ps_cmd.output() {
                    Ok(out) if out.status.success() => match std::fs::read(&tmp) {
                        Ok(bytes) => {
                            let _ = std::fs::remove_file(&tmp);
                            if bytes.is_empty() {
                                return Err("clipboard_get_image: empty image".to_string());
                            }
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            Ok(format!("data:image/png;base64,{b64}"))
                        }
                        Err(e) => Err(format!("clipboard_get_image: read temp file: {e}")),
                    },
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        if stderr.contains("CLIPBOARD_EMPTY") {
                            return Err("clipboard_get_image: clipboard has no image (screenshot likely failed)".to_string());
                        }
                        Err(format!("clipboard_get_image: powershell failed: {stderr}"))
                    }
                    Err(e) => Err(format!("clipboard_get_image: powershell spawn failed: {e}")),
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
